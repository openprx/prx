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
pub use crate::chat::tui::{ConversationLine, StreamingDraft, TuiInput};

/// 占位：TuiInput（非 terminal-tui feature；保持 reducer 在最小 feature 下也能编译）
#[cfg(not(feature = "terminal-tui"))]
pub type TuiInput = Vec<String>;

/// 占位：ConversationLine（非 terminal-tui feature）
#[cfg(not(feature = "terminal-tui"))]
pub type ConversationLine = String;

/// 占位：StreamingDraft（非 terminal-tui feature）
#[cfg(not(feature = "terminal-tui"))]
pub type StreamingDraft = (String, String, u64);

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::ChatMode;
use crate::channels::traits::SendMessage;
use crate::chat::action::{Action, CompactReason, HistoryDir};
use crate::chat::session::{ChatSession, ChatTurn};
use crate::hooks::HookEvent;
use crate::memory::MemoryCategory;
use crate::providers::ChatMessage;
use crate::util::truncate_with_ellipsis;

/// S2-B Step 1: `Action::HistoryCompacted` reducer 对齐 `chat::mod::compact_chat_history`
/// 的常量边界。三个常量必须与 `chat::mod` 同源以确保两条路径在双写期产生相同结果；
/// 后续 step 删除旧路径时直接用 reducer 这套即可。
const COMPACT_KEEP_MESSAGES: usize = 8;
const COMPACT_CONTENT_CHARS: usize = 320;
const COMPACT_TOTAL_CHARS: usize = 2400;

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
        draft_id: String,
        history: Vec<ChatMessage>,
        cancel: CancellationToken,
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
        tool_id: String,
        name: String,
        args: String,
    },
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
}

/// TUI UI 临时状态（退出即弃，不持久化）.
///
/// Step 2 起接入真实 `TuiInput`/`ConversationLine`（feature = "terminal-tui"）；
/// 非 TUI feature 下使用占位类型保持编译兼容。
#[allow(dead_code)]
pub struct UiState {
    /// 渲染好的对话行
    pub conversation_lines: Vec<ConversationLine>,
    /// 多行输入 buffer + 历史
    pub input: TuiInput,
    /// 当前对话回合计数（用于状态栏）
    pub turn_count: usize,
    /// 是否启用 ASCII 降级（非 UTF-8 终端）
    pub ascii_fallback: bool,
    /// 上次 Ctrl+C 的时间戳（ms），用于双击窗口判断
    pub last_ctrlc_ms: u64,
    /// 最近一次输入提交（reducer 内 KeyPressed::Enter 时由 reduce 自身派生
    /// `Action::InputSubmitted`；该字段用于测试断言双写期最后一次提交内容）
    pub last_submitted: Option<String>,
}

/// 不可变 UI 快照（renderer 仅读，dispatcher 在 ui_dirty=true 时构造）.
///
/// S4-A Commit 1: 引入 UiSnapshot 作为 reducer 与 ratatui 渲染线程之间的
/// 单向只读通道。Arc 字段共享让"每轮 push 一行"不需要 clone 整个
/// `Vec<ConversationLine>`；revision 单调递增供 watch::Sender::send_if_modified
/// 跳过相同帧 + 调试断言。
///
/// 字段对应渲染 chrome 需要的最小集（status bar / streaming preview / input 框
/// / footer）；BottomChromeView trait（Commit 2 落地）抽象掉 TuiState vs
/// UiSnapshot 的差异，让 render_bottom_chrome 双源共用。
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
    /// 会话标题（status bar 显示）.
    pub session_title: Arc<str>,
    /// 对话回合计数（status bar 显示）.
    pub turn_count: usize,
    /// ASCII 降级模式标志.
    pub ascii_fallback: bool,
    /// 对话行历史（renderer 增量 insert_before 用 len() diff）.
    pub conversation_lines: Arc<Vec<ConversationLine>>,
    /// 当前 in-flight streaming draft（None 表示空闲）.
    pub streaming: Option<StreamingDraft>,
    /// 输入 buffer 快照（clone 成本接受，多行场景 < INPUT_MAX_VISIBLE_ROWS）.
    pub input: TuiInput,
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
            session_title: Arc::from(""),
            turn_count: 0,
            ascii_fallback: false,
            conversation_lines: Arc::new(Vec::new()),
            streaming: None,
            input: TuiInput::new(),
        }
    }
}

/// 流式推理中间态（每轮重置）.
#[allow(dead_code)]
pub struct StreamState {
    /// 当前 in-flight streaming draft（Step 3 起由 reducer 接管版本号防护）
    pub draft: Option<StreamingDraft>,
    /// 当前回合中处于 Running 状态的工具卡片索引列表
    pub pending_tool_cards: Vec<usize>,
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
    /// S2.5 P1-B: 本轮累积的 tool_calls — ToolStarted/Finished 期间累积，
    /// RecordAssistantTurn 用 mem::take 回填到 session.turns.last_mut().tool_calls.
    pub current_turn_tool_calls: Vec<crate::chat::session::ToolCallSummary>,
    /// S2.5 P1-B: ToolStarted 的 args_preview 暂存 — ToolFinished 时 remove 并 push
    /// 到 current_turn_tool_calls（key 用 tool name，回避 ToolStarted 缺 id 的局限）.
    pub current_turn_tool_args: std::collections::HashMap<String, String>,
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
}

impl ChatState {
    /// 构造初始状态（合理默认值）.
    ///
    /// `provider`/`model` 传入 Arc<str> 以避免后续 clone。
    /// `shutdown` 由调用方创建并共享给所有子任务。
    pub fn new(provider: Arc<str>, model: Arc<str>, shutdown: CancellationToken) -> Self {
        Self {
            session: SessionState {
                id: String::new(),
                title: String::new(),
                provider,
                model,
                mode: ChatMode::default(),
                turns: Vec::new(),
                history: Vec::new(),
            },
            ui: UiState {
                conversation_lines: Vec::new(),
                input: Self::new_input(),
                turn_count: 0,
                ascii_fallback: false,
                last_ctrlc_ms: 0,
                last_submitted: None,
            },
            stream: StreamState {
                draft: None,
                pending_tool_cards: Vec::new(),
            },
            control: ControlState {
                active_cancel: None,
                shutdown,
                generating: false,
                current_turn_tool_calls: Vec::new(),
                current_turn_tool_args: std::collections::HashMap::new(),
            },
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

    /// 构造当前状态对应的 [`UiSnapshot`].
    ///
    /// S4-A Commit 1: 由 dispatcher 在 `reduce_tracked` 返回 ui_dirty=true 后调用，
    /// 通过 `watch::Sender::send_if_modified` 推送给 ratatui 渲染线程。
    /// Arc 字段（`conversation_lines`）让快照之间共享底层 Vec，避免每轮 push
    /// 一行就整体 clone；session_title 是短 String，转 Arc<str> 后 clone 是
    /// refcount 增量。
    ///
    /// `revision` 必须由调用方维护单调递增（dispatcher 自带 `AtomicU64`），
    /// 这样 receiver 端可断言 `new.revision > cur.revision` 跳过乱序帧。
    #[cfg(feature = "terminal-tui")]
    #[must_use]
    #[allow(dead_code)]
    pub fn build_ui_snapshot(&self, revision: u64) -> UiSnapshot {
        UiSnapshot {
            revision,
            provider: Arc::clone(&self.session.provider),
            model: Arc::clone(&self.session.model),
            session_title: Arc::from(self.session.title.as_str()),
            turn_count: self.ui.turn_count,
            ascii_fallback: self.ui.ascii_fallback,
            conversation_lines: Arc::new(self.ui.conversation_lines.clone()),
            streaming: self.stream.draft.clone(),
            input: self.ui.input.clone(),
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
        // 对于可能动态决定 dirty 的 Action（如 KeyPressed），下面追加运行时校正.
        let snap_before = self.snapshot_dirty_fields();
        let effects = self.reduce(action);
        let snap_after = self.snapshot_dirty_fields();
        // 若静态 whitelist 已 true 直接返回；否则用运行时 diff 兜底（KeyPressed
        // 等组合 Action 可能命中 dirty 也可能不命中）.
        let dirty_final = dirty || (snap_before != snap_after);
        (effects, dirty_final)
    }

    /// `reduce_tracked` 用于 dirty 判定的运行时兜底：返回 ui.conversation_lines.len() /
    /// stream.draft.is_some() 等粒度指纹.
    ///
    /// 注：仅对**长度/计数级**变化敏感（如 push 一行 / draft None→Some），不对
    /// 内容字节级变化敏感（如 streaming chunk 累积）— streaming 的内容变化由
    /// 静态 whitelist `ui_dirty_for` 兜住（StreamChunkReceived → true）.
    #[cfg(feature = "terminal-tui")]
    fn snapshot_dirty_fields(&self) -> (usize, bool, u64, usize) {
        let draft_ver = self.stream.draft.as_ref().map_or(0, |d| d.version);
        (
            self.ui.conversation_lines.len(),
            self.stream.draft.is_some(),
            draft_ver,
            self.ui.input.lines.len(),
        )
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
            Action::HistoryNavigated(dir) => self.reduce_history_navigated(dir),
            Action::InputCancelled => self.reduce_input_cancelled(),

            // ── 槽命令 ────────────────────────────────────────────
            Action::SlashCommandIssued { cmd: _cmd, args: _args } => {
                // Step 4: 分发到 commands 模块处理
                vec![]
            }
            Action::ModeChanged(mode) => {
                // Step 2: ModeChanged reducer 已实现，但主循环尚无 dispatch 来源。
                // 由 SlashCommand 路径转发，Step 5 接入；当前分支供回归测试用。
                self.session.mode = mode;
                vec![Effect::RequestRedraw]
            }
            Action::HistoryCleared => self.reduce_history_cleared(),
            Action::HistoryCompacted { reason } => self.reduce_history_compacted(reason),

            // ── LLM 流式 (Step 3) ─────────────────────────────────
            Action::TurnStarted { draft_id, cancel } => self.reduce_turn_started(draft_id, cancel),
            Action::StartLLMTurn {
                draft_id,
                history,
                cancel,
            } => self.reduce_start_llm_turn(draft_id, history, cancel),
            Action::StreamChunkReceived {
                draft_id,
                delta,
                version,
            } => self.reduce_stream_chunk_received(&draft_id, &delta, version),
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => self.reduce_stream_completed(&draft_id, final_text, reasoning),
            Action::StreamFailed {
                draft_id,
                err,
                retryable,
            } => self.reduce_stream_failed(&draft_id, err, retryable),
            Action::StreamCancelled { draft_id } => self.reduce_stream_cancelled(&draft_id),

            // ── 工具事件 (Step 3) ─────────────────────────────────
            Action::ToolStarted { name, args } => self.reduce_tool_started(name, args),
            Action::ToolFinished {
                name,
                success,
                duration_ms,
                result,
            } => self.reduce_tool_finished(name, success, duration_ms, result),
            Action::ToolProgress { iteration, max } => self.reduce_tool_progress(iteration, max),
            Action::ToolApprovalRequested { tool_id, name, args } => {
                self.reduce_tool_approval_requested(tool_id, name, args)
            }
            Action::ToolApprovalReceived { tool_id, approved } => {
                self.reduce_tool_approval_received(&tool_id, approved)
            }
            Action::StreamRetryAttempt { attempt, reason } => self.reduce_stream_retry_attempt(attempt, &reason),

            // ── 会话 ──────────────────────────────────────────────
            Action::SessionLoaded(session) => self.reduce_session_loaded(session),
            Action::SessionSaved { id } => self.reduce_session_saved(id),
            Action::SessionSwitched { id } => self.reduce_session_switched(id),
            Action::RecordUserTurn(content) => self.reduce_record_user_turn(content),
            Action::RecordAssistantTurn(content) => self.reduce_record_assistant_turn(content),
            Action::RecordSystemMessage { content } => self.reduce_record_system_message(content),
            Action::SetLeadingSystemPrompt { content } => self.reduce_set_leading_system_prompt(content),

            // ── UI 折叠/展开 ────────────────────────────────────
            Action::ToolCardFoldToggled => self.reduce_tool_card_fold_toggled(),
            Action::ReasoningFoldToggled => self.reduce_reasoning_fold_toggled(),
            Action::RedrawRequested => vec![Effect::RequestRedraw],
            Action::SystemMessageAdded { text } => self.reduce_system_message_added(text),

            // ── 退出 ──────────────────────────────────────────────
            Action::CancelRequested => self.reduce_cancel_requested(),
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

        // Tab → 折叠/展开最近 ToolResult 卡片
        if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE {
            return self.reduce_tool_card_fold_toggled();
        }
        // Ctrl+R → 折叠/展开 Reasoning 卡片
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            return self.reduce_reasoning_fold_toggled();
        }
        // Ctrl+L → 清屏（请求重绘即可，host 终端清屏由 effect 执行器决定）
        if key.code == KeyCode::Char('l') && key.modifiers == KeyModifiers::CONTROL {
            return vec![Effect::RequestRedraw];
        }
        // Ctrl+C → 单击取消 / 双击退出（500ms 窗口）
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            let prev = self.ui.last_ctrlc_ms;
            self.ui.last_ctrlc_ms = now_ms;
            if prev != 0 && now_ms.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                // 双击 → 优雅退出。Effect::Quit 由外壳触发 shutdown_token.cancel()
                return vec![Effect::Quit];
            }
            // 单击 → 仅记录窗口；实际 cancel 由外壳读取 active_cancel
            return vec![];
        }
        // Ctrl+D → 空 buffer 退出 / 非空 forward-delete（委托 handle_key）
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
            if self.ui.input.is_empty() {
                return vec![Effect::Quit];
            }
            // 非空 buffer 转发为 Delete
            let synthetic = crossterm::event::KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
            let _ = self.ui.input.handle_key(synthetic);
            return vec![Effect::RequestRedraw];
        }
        // 其他键 → 转发到 input buffer，根据 InputOutcome 派生后续 Action 自递归
        match self.ui.input.handle_key(key) {
            crate::chat::tui::InputOutcome::Submitted(text) => {
                // 用 reduce_with_now 重入以保持单一处理路径
                self.reduce_input_submitted(text)
            }
            crate::chat::tui::InputOutcome::Cancelled => self.reduce_input_cancelled(),
            crate::chat::tui::InputOutcome::Consumed | crate::chat::tui::InputOutcome::Unhandled => {
                vec![Effect::RequestRedraw]
            }
        }
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
        self.ui.input.paste(text);
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
        self.ui.turn_count = self.ui.turn_count.saturating_add(1);
        let log_msg = format!("input_submitted len={}", text.chars().count());
        self.ui.last_submitted = Some(text);
        // S2.5 P1-B: 兜底清理本轮 tool_calls 缓冲（幂等：新一轮 turn 入口前清空，
        // 防御上一轮 stream 异常终止时未走 Completed/Cancelled/Failed 清理路径）.
        self.control.current_turn_tool_calls.clear();
        self.control.current_turn_tool_args.clear();
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: log_msg,
            },
            Effect::RequestRedraw,
        ]
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
        self.ui.input.clear();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_input_cancelled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// 处理 Tab — 折叠/展开最近 ToolResult.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_card_fold_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                break;
            }
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
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::Reasoning { folded, .. } = line {
                *folded = !*folded;
                break;
            }
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

    /// `Action::TurnStarted` — 初始化 streaming draft + 注册取消令牌.
    #[cfg(feature = "terminal-tui")]
    fn reduce_turn_started(&mut self, draft_id: String, cancel: CancellationToken) -> Vec<Effect> {
        self.stream.draft = Some(StreamingDraft {
            draft_id: draft_id.clone(),
            accumulated: String::new(),
            version: 0,
        });
        self.control.active_cancel = Some(cancel);
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
        // 占位 feature 下 StreamingDraft = (String, String, u64)
        self.stream.draft = Some((draft_id.clone(), String::new(), 0));
        self.control.active_cancel = Some(cancel);
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
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        cancel: CancellationToken,
    ) -> Vec<Effect> {
        self.stream.draft = Some(StreamingDraft {
            draft_id: draft_id.clone(),
            accumulated: String::new(),
            version: 0,
        });
        self.control.active_cancel = Some(cancel.clone());
        self.control.generating = true;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                draft_id,
                history,
                cancel,
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_start_llm_turn(
        &mut self,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        cancel: CancellationToken,
    ) -> Vec<Effect> {
        self.stream.draft = Some((draft_id.clone(), String::new(), 0));
        self.control.active_cancel = Some(cancel.clone());
        self.control.generating = true;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                draft_id,
                history,
                cancel,
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
        let Some(draft) = self.stream.draft.as_mut() else {
            // 已 finalize — chunk 视为 stale，丢弃
            return vec![];
        };
        if draft.draft_id != draft_id {
            // 跨 turn stale
            return vec![];
        }
        if version <= draft.version {
            // 严格单调：等于或更小都视为乱序/重复，丢弃
            return vec![];
        }
        draft.accumulated.push_str(delta);
        draft.version = version;
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_stream_chunk_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(draft) = self.stream.draft.as_mut() else {
            return vec![];
        };
        if draft.0 != draft_id {
            return vec![];
        }
        if version <= draft.2 {
            return vec![];
        }
        draft.1.push_str(delta);
        draft.2 = version;
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
        let matches = self.stream.draft.as_ref().is_some_and(|d| d.draft_id == draft_id);
        if !matches {
            return vec![];
        }
        self.stream.draft = None;
        self.stream.pending_tool_cards.clear();
        self.control.active_cancel = None;
        self.control.generating = false;
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
        // S2.5 P1-B: 兜底清理本轮 tool 缓冲（RecordAssistantTurn 正常已 mem::take 清空，
        // 此处防御 driver 漏发 RecordAssistantTurn 的边缘情况）.
        self.control.current_turn_tool_calls.clear();
        self.control.current_turn_tool_args.clear();
        effects
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_stream_completed(&mut self, draft_id: &str, final_text: String, reasoning: String) -> Vec<Effect> {
        let matches = self.stream.draft.as_ref().is_some_and(|d| d.0 == draft_id);
        if !matches {
            return vec![];
        }
        self.stream.draft = None;
        self.stream.pending_tool_cards.clear();
        self.control.active_cancel = None;
        self.control.generating = false;
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
        // S2.5 P1-B: 同 terminal-tui 分支兜底清理.
        self.control.current_turn_tool_calls.clear();
        self.control.current_turn_tool_args.clear();
        effects
    }

    /// `Action::StreamFailed` — 清除 draft + LogTrace + NotifyHook(Error).
    ///
    /// Phase F：与旧路径在 chat::run 主循环里 `hooks.emit(HookEvent::Error, payload_error(...))`
    /// 的语义保持一致 — failed turn 必须触发 Error hook，否则外部审计 / webhook 会漏报。
    /// retryable 字段由 EffectExecutor 上层（chat::run 主循环重试逻辑）观察决定是否重发；
    /// hook 一律触发，因为对外可见的"本轮失败"是确定事件。
    fn reduce_stream_failed(&mut self, draft_id: &str, err: String, retryable: bool) -> Vec<Effect> {
        let matches = Self::stream_draft_id_matches(self.stream.draft.as_ref(), draft_id);
        if !matches {
            return vec![];
        }
        self.stream.draft = None;
        self.stream.pending_tool_cards.clear();
        self.control.active_cancel = None;
        self.control.generating = false;
        // S2.5 P1-B: stream 失败丢弃本轮 tool 缓冲（无 RecordAssistantTurn 可回填，
        // 失败后本轮 tool_calls 不可信，下轮入口由 reduce_input_submitted 兜底再清一次）.
        self.control.current_turn_tool_calls.clear();
        self.control.current_turn_tool_args.clear();
        vec![
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!("stream_failed draft_id={draft_id} retryable={retryable} err={err}"),
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
        let matches = Self::stream_draft_id_matches(self.stream.draft.as_ref(), draft_id);
        if !matches {
            return vec![];
        }
        self.stream.draft = None;
        self.stream.pending_tool_cards.clear();
        self.control.active_cancel = None;
        self.control.generating = false;
        // S2.5 P1-B: cancel 丢弃本轮 tool 缓冲（同 failed 路径语义）.
        self.control.current_turn_tool_calls.clear();
        self.control.current_turn_tool_args.clear();
        vec![Effect::RequestRedraw]
    }

    /// `Action::ToolStarted` — 追加 Running 状态的 ToolResult 卡片 + 记录索引.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_started(&mut self, name: String, args: String) -> Vec<Effect> {
        use crate::chat::tui::{ConversationLine, ToolStatus};
        let args_preview = if args.chars().count() > 80 {
            let prefix: String = args.chars().take(80).collect();
            format!("{prefix}…")
        } else {
            args.clone()
        };
        // S2.5 P1-B: 暂存 args_preview，ToolFinished 时取出 push 到 current_turn_tool_calls.
        self.control
            .current_turn_tool_args
            .insert(name.clone(), args_preview.clone());
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
        self.stream.pending_tool_cards.push(idx);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_tool_started(&mut self, name: String, args: String) -> Vec<Effect> {
        // S2.5 P1-B: 占位 feature 下也累积 args_preview，便于 parity 测试在两种 feature 走通.
        let args_preview = if args.chars().count() > 80 {
            let prefix: String = args.chars().take(80).collect();
            format!("{prefix}…")
        } else {
            args.clone()
        };
        self.control.current_turn_tool_args.insert(name.clone(), args_preview);
        self.ui.conversation_lines.push(format!("tool_started:{name}:{args}"));
        let idx = self.ui.conversation_lines.len().saturating_sub(1);
        self.stream.pending_tool_cards.push(idx);
        vec![Effect::RequestRedraw]
    }

    /// `Action::ToolFinished` — 更新对应 Running 卡片 → Done/Error.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_finished(
        &mut self,
        name: String,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        use crate::chat::tui::{ConversationLine, ToolStatus};
        // S2.5 P1-B: 取出 ToolStarted 暂存的 args_preview（没有则空串，例如 driver
        // 在 supervised 模式下漏发 ToolStarted 的边缘情况），push 到本轮累积列表.
        let args_preview = self.control.current_turn_tool_args.remove(&name).unwrap_or_default();
        self.control.current_turn_tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
        });
        // 第 1 步：从 pending_tool_cards 反向查找最近一个 name 匹配 + Running 的卡片
        // （只借用 conversation_lines，不持 mut 引用，避免 result 跨循环 move 冲突）
        let target_pos = self
            .stream
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
            self.stream.pending_tool_cards.remove(pending_pos);
        }
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
        name: String,
        success: bool,
        duration_ms: u64,
        _result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        // S2.5 P1-B: 同 terminal-tui 分支，回填本轮累积列表.
        let args_preview = self.control.current_turn_tool_args.remove(&name).unwrap_or_default();
        self.control.current_turn_tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
        });
        // 占位 feature 下仅记录 + 弹出最后一个 pending 索引
        if !self.stream.pending_tool_cards.is_empty() {
            self.stream.pending_tool_cards.pop();
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
    fn reduce_tool_progress(&self, iteration: usize, max: usize) -> Vec<Effect> {
        let _ = &self.ui; // 强制依赖 self 防止变 const fn
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_progress {iteration}/{max}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalRequested` — 仅产生 `Effect::RequestApproval`.
    ///
    /// driver 在 supervised autonomy 模式下，**先于** ToolStarted 发送该 Action，
    /// 让 reducer 把请求转给 EffectExecutor / UI；driver 自己通过 oneshot rx
    /// 等响应（dispatcher 把 `ToolApprovalReceived` 转写到 driver 的接收 channel）。
    /// reducer 不维护 pending_approvals — driver 是单一拥有者。
    fn reduce_tool_approval_requested(&self, tool_id: String, name: String, args: String) -> Vec<Effect> {
        let _ = &self.ui;
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_approval_requested tool_id={tool_id} name={name}"),
            },
            Effect::RequestApproval { tool_id, name, args },
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalReceived` — driver 收到 approval 决策后通过
    /// 反向 mpsc 走 dispatcher 路径转回 driver；reducer 端仅做 trace 记账.
    fn reduce_tool_approval_received(&self, tool_id: &str, approved: bool) -> Vec<Effect> {
        let _ = &self.ui;
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("tool_approval_received tool_id={tool_id} approved={approved}"),
        }]
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

    /// Helper：判断当前 draft 的 id 是否匹配传入值（处理 terminal-tui 和占位两种 StreamingDraft 类型）.
    #[cfg(feature = "terminal-tui")]
    fn stream_draft_id_matches(draft: Option<&StreamingDraft>, draft_id: &str) -> bool {
        draft.is_some_and(|d| d.draft_id == draft_id)
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn stream_draft_id_matches(draft: Option<&StreamingDraft>, draft_id: &str) -> bool {
        draft.is_some_and(|d| d.0 == draft_id)
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
        // 取出 draft id（如有）+ cancel token（如有）用于 Effect
        let draft_id_opt = Self::take_draft_id(&self.stream);
        let cancel_opt = self.control.active_cancel.take();
        // 清除流式状态
        self.stream.draft = None;
        self.control.generating = false;

        let mut effects = Vec::new();
        // 优先发 CancelToken 真触发底层取消；再发 CancelDraft 同步 channel UI。
        if let Some(token) = cancel_opt {
            effects.push(Effect::CancelToken(token));
        }
        if let Some(draft_id) = draft_id_opt {
            effects.push(Effect::CancelDraft(draft_id));
        }
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
        let (draft_id_opt, cancel_opt) = if self.control.generating {
            let id = Self::take_draft_id(&self.stream);
            let tok = self.control.active_cancel.take();
            self.stream.draft = None;
            self.control.generating = false;
            (id, tok)
        } else {
            (None, None)
        };

        let mut effects = Vec::new();
        if let Some(token) = cancel_opt {
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
        self.session.id = loaded.id;
        self.session.title = loaded.title;
        self.session.provider = Arc::from(loaded.provider.as_str());
        self.session.model = Arc::from(loaded.model.as_str());
        self.session.mode = loaded.mode;
        self.session.turns = loaded.turns;
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
        ChatSession {
            id: self.session.id.clone(),
            schema_version: crate::chat::session::SCHEMA_VERSION,
            title: self.session.title.clone(),
            provider: self.session.provider.as_ref().to_owned(),
            model: self.session.model.as_ref().to_owned(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            turns: self.session.turns.clone(),
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
        self.session.turns.push(crate::chat::session::ChatTurn {
            role: "user".to_string(),
            content: content.clone(),
            timestamp: chrono::Utc::now(),
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

    /// `Action::RecordAssistantTurn(text)` — 请求 reducer 持久化助手回合到 session 记录和 LLM history.
    ///
    /// 对齐 `session.add_assistant_turn` 语义：
    /// - `updated_at` 由 effect executor 在构建 `SaveSession` 快照时设置（SessionState 不含时间戳）
    /// - S2.5 P1-B: tool_calls 由 reducer 内 ControlState.current_turn_tool_calls
    ///   缓冲 mem::take 回填（关闭原 FIXME(S2.5)，方案 C reducer 回填，不改 Action 签名）.
    fn reduce_record_assistant_turn(&mut self, content: String) -> Vec<Effect> {
        let tool_calls = std::mem::take(&mut self.control.current_turn_tool_calls);
        self.control.current_turn_tool_args.clear();
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
    /// - history 非空但首位**不**是 system → 仍然替换 `history[0]`（与 legacy `first_mut`
    ///   一致；理论上首位应为 system，若不是已是 invariant 违反但 reducer 保持兼容）
    fn reduce_set_leading_system_prompt(&mut self, content: String) -> Vec<Effect> {
        if self.session.history.is_empty() {
            self.session.history.push(ChatMessage::system(content));
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
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "History cleared".to_string(),
            },
        ]
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

    /// 辅助：从 StreamState 中取出当前 draft 的 id（不同 feature 下结构不同）.
    #[cfg(feature = "terminal-tui")]
    fn take_draft_id(stream: &StreamState) -> Option<String> {
        stream.draft.as_ref().map(|d| d.draft_id.clone())
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn take_draft_id(stream: &StreamState) -> Option<String> {
        stream.draft.as_ref().map(|d| d.0.clone())
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
        | Action::HistoryNavigated(_)
        | Action::InputCancelled => true,

        // 终端尺寸变化：snapshot 字段不变（width/height 不在 snapshot 内），
        // 但渲染需要重新布局 — dirty=true 让 watch 触发 redraw 兜底.
        Action::TerminalResized { .. } => true,

        // UI 折叠/展开：直接 mutate conversation_lines → dirty
        Action::ToolCardFoldToggled | Action::ReasoningFoldToggled => true,

        // 槽命令本身 reducer 是 no-op（实际执行在 mod.rs），不变 UI
        Action::SlashCommandIssued { .. } => false,
        // 模式切换：仅写 session.mode，UI 不显示模式（status bar 没 mode 字段）
        Action::ModeChanged(_) => false,

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
        Action::ToolProgress { .. }
        | Action::ToolApprovalRequested { .. }
        | Action::ToolApprovalReceived { .. }
        | Action::StreamRetryAttempt { .. } => false,

        // 会话：SessionLoaded 重建 history + 可能要求 UI 重置；SessionSaved/Switched 不影响 UI
        Action::SessionLoaded(_) => true,
        Action::SessionSaved { .. } | Action::SessionSwitched { .. } => false,
        // Record* 写 session.turns / history，不直接进 conversation_lines（那是
        // ToolStarted/StreamCompleted 等单独处理），UI 不变.
        Action::RecordUserTurn(_)
        | Action::RecordAssistantTurn(_)
        | Action::RecordSystemMessage { .. }
        | Action::SetLeadingSystemPrompt { .. }
        | Action::HistoryCompacted { .. } => false,

        // UI 镜像账本 / 历史清空：直接动 conversation_lines → dirty
        Action::SystemMessageAdded { .. } | Action::HistoryCleared => true,
        // RedrawRequested 仅产生 RequestRedraw Effect，本身不变 snapshot 字段；
        // 但语义上需要触发 redraw — 标 dirty 走 watch 路径.
        Action::RedrawRequested => true,

        // 退出：CancelRequested / ShutdownRequested 可能清空 stream.draft → dirty.
        Action::CancelRequested | Action::ShutdownRequested => true,
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
        assert!(state.session.id.is_empty());
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
        assert!(state.stream.draft.is_none());
        assert!(state.stream.pending_tool_cards.is_empty());
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

        /// 额外：ReasoningFoldToggled 在无 reasoning 卡片时也返回 RequestRedraw
        #[test]
        fn test_reduce_reasoning_fold_toggled_no_panic_when_absent() {
            let mut state = s();
            let effects = state.reduce(Action::ReasoningFoldToggled);
            assert!(has_request_redraw(&effects));
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
            assert!(state.stream.draft.is_none());
            assert!(state.control.active_cancel.is_none());
            assert!(!state.control.generating);
            let effects = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.stream.draft.is_some());
            assert_eq!(
                state.stream.draft.as_ref().map(|d| d.draft_id.clone()),
                Some("d1".to_string())
            );
            assert_eq!(state.stream.draft.as_ref().map(|d| d.version), Some(0));
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
                state.stream.draft.as_ref().map(|d| d.accumulated.clone()),
                Some("hello".to_string())
            );
            assert_eq!(state.stream.draft.as_ref().map(|d| d.version), Some(1));

            // 第二个有效 chunk → 累积
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: " world".to_string(),
                version: 2,
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.stream.draft.as_ref().map(|d| d.accumulated.clone()),
                Some("hello world".to_string())
            );
            assert_eq!(state.stream.draft.as_ref().map(|d| d.version), Some(2));
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
                state.stream.draft.as_ref().map(|d| d.accumulated.clone()),
                Some("AB".to_string())
            );
            assert_eq!(state.stream.draft.as_ref().map(|d| d.version), Some(2));
            // 后来才到 version=1 → 丢弃
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "STALE".to_string(),
                version: 1,
            });
            assert!(effects.is_empty(), "stale version 应返回空 effects");
            assert_eq!(
                state.stream.draft.as_ref().map(|d| d.accumulated.clone()),
                Some("AB".to_string()),
                "accumulated 应保持不变"
            );
            assert_eq!(state.stream.draft.as_ref().map(|d| d.version), Some(2));

            // 重复 version=2 → 也丢弃（strict-monotonic）
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "DUP".to_string(),
                version: 2,
            });
            assert!(effects.is_empty(), "重复 version 应丢弃");
            assert_eq!(
                state.stream.draft.as_ref().map(|d| d.accumulated.clone()),
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
                state.stream.draft.as_ref().map(|d| d.accumulated.clone()),
                Some("ok".to_string())
            );
            assert_eq!(state.stream.draft.as_ref().map(|d| d.version), Some(1));
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
            assert!(state.stream.draft.is_none(), "finalize 后 draft 应清空");
            // 此后 chunk 视为 stale
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "LATE".to_string(),
                version: 2,
            });
            assert!(effects.is_empty(), "finalize 后 chunk 应丢弃");
            assert!(state.stream.draft.is_none());
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
            assert!(state.stream.draft.is_none(), "draft 应清空");
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
            assert!(state.stream.draft.is_none());
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
            assert!(state.stream.draft.is_none());
            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.ui.conversation_lines.len(),
                lines_before,
                "cancel 不应 push 任何 conversation line"
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
            assert!(state.stream.draft.is_some(), "原 draft 应保留");
            assert!(state.control.generating, "generating 标志应保留");
        }

        /// Step3-9: ToolStarted → push Running ToolResult + 索引入队
        #[test]
        fn test_redux_tool_started_pushes_card() {
            use crate::chat::tui::{ConversationLine, ToolStatus};
            let mut state = s();
            let effects = state.reduce(Action::ToolStarted {
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.conversation_lines.len(), 1);
            assert_eq!(state.stream.pending_tool_cards.len(), 1);
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
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let effects = state.reduce(Action::ToolFinished {
                name: "shell".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("ok".to_string()),
            });
            assert!(has_request_redraw(&effects));
            assert!(state.stream.pending_tool_cards.is_empty(), "pending 应被清空");
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
                name: "shell".to_string(),
                args: r#"{}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
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

        /// Step3-11: ToolProgress 仅返回 RequestRedraw + LogTrace
        #[test]
        fn test_redux_tool_progress_returns_log_and_redraw() {
            let mut state = s();
            let effects = state.reduce(Action::ToolProgress { iteration: 3, max: 10 });
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
            // 不 mutate conversation_lines
            assert!(state.ui.conversation_lines.is_empty());
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
            assert!(state.stream.draft.is_none());
            // 重试：开一个新 turn (相同 draft_id 也 OK，draft.version 从 0 起)
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert_eq!(state.stream.draft.as_ref().map(|d| d.version), Some(0));
            // version=1 应被接受（不被前一轮的 5 影响 — 因为 draft 已重建）
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "retry".to_string(),
                version: 1,
            });
            assert!(has_request_redraw(&effects), "新 turn 的 v=1 应被接受");
            assert_eq!(
                state.stream.draft.as_ref().map(|d| d.accumulated.clone()),
                Some("retry".to_string())
            );
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
            assert!(state.stream.draft.is_some());

            let effects = state.reduce(Action::CancelRequested);

            // 状态应已清除
            assert!(!state.control.generating, "generating 应清为 false");
            assert!(state.stream.draft.is_none(), "draft 应清空");
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
            assert!(state.stream.draft.is_none(), "draft 应清空");
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
            assert!(has_request_redraw(&effects), "应含 RequestRedraw");
            assert!(has_log_trace(&effects), "应含 LogTrace");
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
            let effects = state.reduce(Action::RecordAssistantTurn("Rust is fast.".to_string()));
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
                draft_id: "draft-1".to_string(),
                history,
                cancel,
            });

            // 状态变更：draft + active_cancel + generating
            assert!(state.stream.draft.is_some(), "stream.draft 必须被设置");
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
                draft_id: "d2".to_string(),
                history: vec![ChatMessage::user("x")],
                cancel: cancel.clone(),
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

        /// Phase A-3: TurnStarted（旧 Action）保持原行为 — 不发射 Effect::StartTurn
        #[test]
        fn test_phase_a_legacy_turn_started_no_start_turn_effect() {
            let mut state = s();
            let effects = state.reduce(Action::TurnStarted {
                draft_id: "legacy".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.stream.draft.is_some(), "TurnStarted 同样初始化 draft");
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
                draft_id: "d5".to_string(),
                history: vec![ChatMessage::user("hi")],
                cancel,
            });
            assert!(state.control.generating);

            let effects = state.reduce(Action::CancelRequested);
            // generating=true → reducer 发 CancelDraft
            assert!(
                effects.iter().any(|e| matches!(e, Effect::CancelDraft(_))),
                "CancelRequested(generating=true) 应发 CancelDraft"
            );
            assert!(!state.control.generating, "cancel 后 generating 必须复位");
            assert!(state.stream.draft.is_none(), "cancel 后 draft 必须清理");
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
                .draft
                .as_ref()
                .map(|d| d.accumulated.clone())
                .expect("test: stream.draft must exist after StreamChunkReceived");
            assert_eq!(
                redux_accumulated, legacy_accumulated,
                "S2-A M2 验收：reducer accumulated 必须等于旧路径 update_draft 传入的累计串"
            );
            assert_eq!(
                state.stream.draft.as_ref().map(|d| d.version),
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
            assert!(state.stream.draft.is_none(), "completed 后 draft 应清空");
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
            let _ = state.reduce(Action::RecordAssistantTurn("answer".to_string()));
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
        fn test_t3_3_fix_a_dispatch_order_snapshot_contract() {
            // ── 正序：RecordAssistantTurn → StreamCompleted ──
            let mut state_a = s();
            state_a.session.id = "sess-fwd".to_string();
            let _ = state_a.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state_a.reduce(Action::TurnStarted {
                draft_id: "d-fwd".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state_a.reduce(Action::RecordAssistantTurn("a-fwd".to_string()));
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
            let _ = state_b.reduce(Action::RecordAssistantTurn("a-rev".to_string()));
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
        fn test_t3_3_fix_a_stream_error_no_save() {
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
        fn test_t3_3_fix_a_stream_cancelled_no_save() {
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
            assert!(state.stream.draft.is_none(), "failed 后 draft 应清空");
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
            assert!(state.stream.draft.is_none(), "cancelled 后 draft 应清空");
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
                name: "search".to_string(),
                args: "{\"q\":\"openprx\"}".to_string(),
            });
            let _ = state_a.reduce(Action::ToolFinished {
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
                name: "search".to_string(),
                args: "{\"q\":\"openprx\"}".to_string(),
            });
            let _ = state_b.reduce(Action::ToolFinished {
                name: "search".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("found 3 results".to_string()),
            });

            // 核心不变量：draft.accumulated 完全相同（tool 事件不污染流式文本）
            let acc_a = state_a
                .stream
                .draft
                .as_ref()
                .map(|d| d.accumulated.clone())
                .expect("test: scenario A draft must exist");
            let acc_b = state_b
                .stream
                .draft
                .as_ref()
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
                state_a.stream.draft.as_ref().map(|d| d.version),
                state_b.stream.draft.as_ref().map(|d| d.version),
                "tool 事件不应推进 stream.version"
            );
            assert_eq!(state_a.stream.draft.as_ref().map(|d| d.version), Some(2));

            // tool 卡片在两边都已落地，且 ToolFinished 后已从 pending 移除
            assert_eq!(
                state_a.stream.pending_tool_cards.len(),
                state_b.stream.pending_tool_cards.len(),
                "两个序列 pending_tool_cards 数量必须一致"
            );
            assert!(
                state_a.stream.pending_tool_cards.is_empty(),
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
            assert!(state.stream.draft.is_none(), "CancelRequested 后 draft 清空");
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

            // Auto
            let _ = state.reduce(Action::ModeChanged(ChatMode::Auto));
            legacy.set_mode(ChatMode::Auto);
            assert_eq!(state.session.mode, legacy.mode, "Auto 模式应一致");

            // Edit (default)
            let _ = state.reduce(Action::ModeChanged(ChatMode::Edit));
            legacy.set_mode(ChatMode::Edit);
            assert_eq!(state.session.mode, legacy.mode, "Edit 模式应一致");
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
                    let _ = state.reduce(Action::RecordAssistantTurn(text.to_string()));
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
            let _ = state.reduce(Action::RecordAssistantTurn("a1".to_string()));
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

            let _ = state.reduce(Action::RecordAssistantTurn("hi back".to_string()));
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
            assert!(state.stream.draft.is_none());

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
            assert!(state.stream.draft.is_none());

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

        /// S2-B-7 (Codex 阻塞): both_mode_legacy_arc_cancel_token_sync
        ///
        /// ReduxMode::Both 期间，chat::run 同时:
        /// - 把 `cancellation.clone()` 写入 legacy `Arc<Mutex<Option<CancellationToken>>>`
        ///   (mod.rs:1529 `*active_cancel.lock() = Some(cancellation.clone())`)
        /// - 把 `cancellation.clone()` 通过 `Action::TurnStarted { cancel }` 写入
        ///   `state.control.active_cancel`
        ///
        /// 这两个 token 必须是**同一个 cancellation 实例的克隆**（共享 internal
        /// cancellation state），否则会出现"UI 取消了但底层仍跑"的窗口 —
        /// 用户按 Ctrl+C → reducer 发 `Effect::CancelToken(state_clone)` →
        /// executor 调 `state_clone.cancel()` →
        /// 但旧顶层 Ctrl+C handler 读 `legacy_arc.lock().as_ref().cancel()`
        /// 必须**也**看到 cancelled。
        ///
        /// 本测试模拟 Both 模式的双写，验证：
        /// 1. 双写后 legacy Arc 与 state 各自持有 Some(token)
        /// 2. 取消 Effect::CancelToken 携带的 token 后，legacy Arc 中的 token
        ///    `is_cancelled()` 也变 true（共享 cancellation）
        /// 3. 反向同样：取消 legacy Arc 中的 token，state 内的 token 也立即取消
        #[test]
        fn test_s2b_both_mode_legacy_arc_cancel_token_sync() {
            // 模拟 Both 模式：chat::run 创建一份 cancellation，clone 给两条路径
            let cancellation = CancellationToken::new();
            let legacy_arc: Arc<parking_lot::Mutex<Option<CancellationToken>>> =
                Arc::new(parking_lot::Mutex::new(None));

            // 双写 1：写入 legacy Arc（mod.rs:1529 路径）
            *legacy_arc.lock() = Some(cancellation.clone());

            // 双写 2：写入 reducer state（Action::TurnStarted 路径）
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-both".to_string(),
                cancel: cancellation.clone(),
            });

            // 契约 1：双写后两边都持有 Some(token)
            assert!(legacy_arc.lock().is_some(), "legacy Arc 应持有 Some(token)");
            assert!(
                state.control.active_cancel.is_some(),
                "state.control.active_cancel 应持有 Some(token)"
            );
            // 两边都还没 cancel
            assert!(
                legacy_arc
                    .lock()
                    .as_ref()
                    .is_some_and(|t| !CancellationToken::is_cancelled(t)),
                "legacy Arc 中的 token 应处于未取消状态"
            );
            assert!(
                state
                    .control
                    .active_cancel
                    .as_ref()
                    .is_some_and(|t| !CancellationToken::is_cancelled(t)),
                "state 端 token 应处于未取消状态"
            );

            // 触发 CancelRequested → reducer take 走 state 端 token、发 Effect::CancelToken
            let effects = state.reduce(Action::CancelRequested);
            let token_from_effect = effects
                .iter()
                .find_map(|e| match e {
                    Effect::CancelToken(t) => Some(t.clone()),
                    _ => None,
                })
                .expect("test: Effect::CancelToken must be present");

            // 关键断言：reducer 已 take 走 state 端，但 legacy Arc 仍持有 Some
            // （Both 模式下 legacy 字段独立写/清，reducer 不动它）
            assert!(state.control.active_cancel.is_none(), "reducer take 后 state 端清空");
            assert!(
                legacy_arc.lock().is_some(),
                "Both 模式下 legacy Arc 仍持有 token（独立轴），不被 reducer 清空"
            );

            // 契约 2：EffectExecutor 真调 token.cancel() —
            // legacy Arc 中的 token 必须**同步**变 cancelled（共享 cancellation 状态）
            token_from_effect.cancel();
            let legacy_is_cancelled = legacy_arc.lock().as_ref().map(CancellationToken::is_cancelled);
            assert_eq!(
                legacy_is_cancelled,
                Some(true),
                "legacy Arc 中的 token 必须与 Effect::CancelToken 的 token 共享 cancellation — \
                 否则会出现 'UI 取消了但底层仍跑' 的窗口"
            );
            // 原始 cancellation 也应同步 cancelled
            assert!(
                cancellation.is_cancelled(),
                "原始 cancellation handle 也应 cancelled（同一 token 克隆）"
            );

            // 契约 3：反向同步 — 新一轮 turn，从 legacy Arc 端 cancel，state 端应同步
            let cancellation_b = CancellationToken::new();
            let legacy_arc_b: Arc<parking_lot::Mutex<Option<CancellationToken>>> =
                Arc::new(parking_lot::Mutex::new(Some(cancellation_b.clone())));
            let mut state_b = s();
            let _ = state_b.reduce(Action::TurnStarted {
                draft_id: "d-both-b".to_string(),
                cancel: cancellation_b.clone(),
            });
            // legacy 端调 cancel
            if let Some(t) = legacy_arc_b.lock().as_ref() {
                t.cancel();
            }
            // state 端应同步 cancelled
            assert!(
                state_b
                    .control
                    .active_cancel
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled),
                "legacy 端 cancel 后 state 端 token 应同步 cancelled（同一 cancellation 克隆）"
            );
            assert!(
                cancellation_b.is_cancelled(),
                "原始 cancellation handle B 也应同步 cancelled"
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
            let _ = state.reduce(Action::RecordAssistantTurn("assistant-r1".to_string()));
            assert_eq!(state.session.history.len(), 3);
            let h2 = state.session.history.get(2).expect("test: history[2] = assistant");
            assert_eq!(h2.role, "assistant");
            assert_eq!(h2.content, "assistant-r1");
            assert_eq!(state.session.turns.len(), 2, "session.turns +1（assistant）");

            // 再来一轮 — 顺序应仍稳定 system, user, assistant, user, assistant
            let _ = state.reduce(Action::RecordUserTurn("user-q2".to_string()));
            let _ = state.reduce(Action::RecordAssistantTurn("assistant-r2".to_string()));
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
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                name: "shell".to_string(),
                success: true,
                duration_ms: 12,
                result: Some("ok".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn("answer".to_string()));

            let last = state.session.turns.last().expect("test: assistant turn");
            assert_eq!(last.role, "assistant");
            assert_eq!(last.tool_calls.len(), 1, "本轮 1 个 tool_call 必须回填");
            let call: &ToolCallSummary = last.tool_calls.first().expect("test: tool_calls[0]");
            assert_eq!(call.name, "shell");
            assert!(call.success);
            assert_eq!(call.args_preview, r#"{"cmd":"ls"}"#);

            // 回填后 ControlState 缓冲必须清空（mem::take + clear）.
            assert!(state.control.current_turn_tool_calls.is_empty());
            assert!(state.control.current_turn_tool_args.is_empty());
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
                    name: name.clone(),
                    args: format!("args-{i}"),
                });
                let _ = state.reduce(Action::ToolFinished {
                    name,
                    success: ok,
                    duration_ms: 10,
                    result: None,
                });
            }
            let _ = state.reduce(Action::RecordAssistantTurn("a".to_string()));

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
                name: "leftover".to_string(),
                args: "x".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                name: "leftover".to_string(),
                success: true,
                duration_ms: 1,
                result: None,
            });
            assert_eq!(state.control.current_turn_tool_calls.len(), 1);
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-p1b-3a".to_string(),
                final_text: "x".to_string(),
                reasoning: String::new(),
            });
            // StreamCompleted 兜底 clear 后缓冲为空.
            assert!(state.control.current_turn_tool_calls.is_empty());
            assert!(state.control.current_turn_tool_args.is_empty());

            // Turn 2：RecordAssistantTurn 应得到空 tool_calls（未被 Turn 1 残留污染）.
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-3b".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn("clean".to_string()));
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
                name: "partial".to_string(),
                args: "...".to_string(),
            });
            // 此时 args 暂存有内容
            assert_eq!(state.control.current_turn_tool_args.len(), 1);

            let _ = state.reduce(Action::StreamCancelled {
                draft_id: "d-p1b-4".to_string(),
            });
            assert!(
                state.control.current_turn_tool_calls.is_empty(),
                "cancel 后缓冲必须清空"
            );
            assert!(
                state.control.current_turn_tool_args.is_empty(),
                "cancel 后 args 暂存必须清空"
            );

            // 同理验证 StreamFailed.
            let mut state2 = s();
            let _ = state2.reduce(Action::TurnStarted {
                draft_id: "d-p1b-4b".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state2.reduce(Action::ToolStarted {
                name: "partial2".to_string(),
                args: "...".to_string(),
            });
            let _ = state2.reduce(Action::StreamFailed {
                draft_id: "d-p1b-4b".to_string(),
                err: "timeout".to_string(),
                retryable: true,
            });
            assert!(state2.control.current_turn_tool_calls.is_empty());
            assert!(state2.control.current_turn_tool_args.is_empty());
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
                name: "search".to_string(),
                args: r#"{"q":"x"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                name: "search".to_string(),
                success: true,
                duration_ms: 5,
                result: Some("hit".to_string()),
            });
            let _ = state.reduce(Action::ToolStarted {
                name: "fetch".to_string(),
                args: "url".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                name: "fetch".to_string(),
                success: false,
                duration_ms: 30,
                result: Some("404".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn("done".to_string()));
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
        fn s4_a_1_ui_dirty_false_on_log_trace_only_actions() {
            let mut state = make_state();
            let (_e, d) = state.reduce_tracked(Action::ToolProgress { iteration: 1, max: 3 });
            assert!(!d, "ToolProgress 仅 LogTrace, 不应 dirty");
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
}
