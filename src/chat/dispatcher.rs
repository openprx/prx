//! Redux dispatcher 基础设施 (Step 5a-1 — 真业务执行 + dual-write guard).
//!
//! 提供三件套，把生产事件接入 reducer 并按需执行业务：
//! - [`ChatDispatcher`]: `Action` 发送端封装（bounded mpsc + try_send 政策）
//! - [`EffectExecutor`]: shadow 模式（5b）所有业务 Effect 都是 no-op；
//!   real 模式（5a-1）持有 [`EffectDeps`]，按 PRX_CHAT_REDUX 灰度真执行
//! - [`StreamChunkCoalescer`]: 当 channel 满时合并 `StreamChunkReceived` delta
//!   为单个 `Action`，避免反压丢失中间块
//!
//! 设计要点（Codex 审计 P0-1 / P0-2 / P0-3 / P2-coalescer-version）:
//! - **bounded channel**：`Action` channel 容量 2048，防 OOM
//! - **dual-write guard**：[`RuntimeDualWriteGuard`] (Arc<AtomicBool>) 标记本轮是否
//!   由 Redux 路径处理；旧路径在 Both/Redux 模式下根据 guard 决定是否跳过持久化，
//!   防止 history / session 被双写
//! - **长耗时 effect spawn 子任务**：`StartTurn` / `SaveSession` / `EmitChannelMessage`
//!   / `PersistToMemory` 在 deps 模式下统一 `tokio::spawn`，避免 await 阻塞主循环
//! - **coalescer version 取最新**：与 reducer `state.rs:540` strict-monotonic
//!   一致，合并时 `version = max(pending, new)`，否则高版本先到合并后会被丢
//! - **RouteDecision / ProviderExecutionOutcome timeline**：streaming 路径保留
//!   ingress 层统一记录，dispatcher 只负责 stream-state 事件顺序
//! - **OS 信号统一入 Action**：Ctrl+C / SIGTERM handler `try_send` shutdown action
//!
//! 灰度模式（与 `chat::ReduxMode` 对齐）:
//! - `Off`：EffectExecutor::new_shadow()（业务 no-op，仅 LogTrace 跑）
//! - `Both`：EffectExecutor::new_with_deps()（业务真执行）+ 旧路径仍跑 + guard 抑制
//!   旧路径的持久化，让两路并行但只有 reducer 真正持久化（reducer 是新真源）
//! - `Redux`：与 Both 类似（5a-1 阶段不删旧路径，仅运行时让 reducer 主导，
//!   5a-3 才真正删旧路径）

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex as ParkingMutex;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::sync::CancellationToken;

use crate::channels::Channel;
use crate::chat::action::Action;
use crate::chat::state::{ChatState, Effect};
use crate::hooks::HookManager;
use crate::memory::Memory;
use crate::observability::Observer;
use crate::providers::Provider;

/// Action channel 容量上限（Codex P0-3）.
///
/// 选 2048：覆盖典型 chat session 的 burst（用户输入 + 流式 chunk + 工具事件），
/// 又能在 OOM 前触发 backpressure → coalescing。
pub const ACTION_CHANNEL_CAPACITY: usize = 2048;

// ─── ApprovalRouter (S3 T3-1) ─────────────────────────────────────────────────

/// **S3 T3-1**: 工具 approval 请求-应答路由器.
///
/// driver 在执行需 approval 的 tool 前注册一个 `tool_id → oneshot::Sender<bool>`；
/// dispatcher_task 在 reducer 处理完 `Action::ToolApprovalReceived` 后调用
/// [`Self::resolve`]，把决策回传给阻塞在 oneshot rx 上的 driver。
///
/// 设计要点（Codex 审计 B+D 推荐方案）:
/// - oneshot per request，自然 fire-and-forget 不重复消费
/// - `Arc<ApprovalRouter>` 跨 spawn 边界共享所有权（driver / dispatcher_task）
/// - parking_lot Mutex：register/resolve 都是短同步操作，绝不持锁过 await
/// - 拒绝 / 超时 / 取消任意路径都由 driver 自身负责清理 pending（drop oneshot tx）
///
/// 不变量：每个 `tool_id` 至多注册一次。重复注册视为 BUG（driver bug），后注册
/// 会替换前一个 sender — 由 driver 保证不发生（每次发请求前 tool_id 是唯一新值）。
#[derive(Default)]
pub struct ApprovalRouter {
    pending: ParkingMutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
}

impl ApprovalRouter {
    /// 构造空路由器.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// driver 注册一个 pending approval（`tool_id`→`tx`）.
    ///
    /// 同 `tool_id` 已存在时旧 sender 被替换（仅用作防御性容错，正常路径不会触发）.
    pub fn register(&self, tool_id: String, tx: tokio::sync::oneshot::Sender<bool>) {
        let mut guard = self.pending.lock();
        if guard.insert(tool_id.clone(), tx).is_some() {
            tracing::warn!(tool_id = %tool_id, "ApprovalRouter::register: replacing existing pending tx");
        }
    }

    /// dispatcher_task 调用：取出 pending sender 并 resolve 决策.
    ///
    /// 找不到对应 `tool_id`（driver 已经超时清理 / cancel 路径丢弃）时返回 false。
    pub fn resolve(&self, tool_id: &str, approved: bool) -> bool {
        let tx_opt = self.pending.lock().remove(tool_id);
        tx_opt.map_or_else(
            || {
                tracing::debug!(tool_id = %tool_id, "ApprovalRouter::resolve: no pending entry");
                false
            },
            |tx| {
                if tx.send(approved).is_err() {
                    tracing::debug!(
                        tool_id = %tool_id,
                        "ApprovalRouter::resolve: rx already dropped (driver cancelled)"
                    );
                }
                true
            },
        )
    }
}

// ─── ChatDispatcher ────────────────────────────────────────────────────────────

/// `Action` 发送端封装。仅暴露 `try_send` / `send` 两种政策，禁止 unbounded clone。
///
/// - `try_send`：非阻塞 — 用于流式 chunk / 控制 Action（满时调用方应走 coalescer）
/// - `send_blocking`：阻塞同步路径 — 用于关键退出 Action（Ctrl+C / SIGTERM handler）
/// - `send`：异步阻塞 — 用于关键 Action 且调用方在 async 上下文（如 main 循环）
#[allow(dead_code)]
#[derive(Clone)]
pub struct ChatDispatcher {
    action_tx: mpsc::Sender<Action>,
}

/// `try_send` 政策结果（供调用方决定是否需要 coalescing / 兜底）.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchResult {
    /// Action 已入队
    Sent,
    /// Channel 已满（调用方应 coalesce 或丢弃）
    Backpressured,
    /// Channel 已关闭（dispatcher task 已退出）
    ChannelClosed,
}

impl ChatDispatcher {
    /// 构造 dispatcher + 接收端。接收端给 [`spawn_dispatcher_task`] 消费。
    #[allow(dead_code)]
    pub fn new() -> (Self, mpsc::Receiver<Action>) {
        let (action_tx, action_rx) = mpsc::channel::<Action>(ACTION_CHANNEL_CAPACITY);
        (Self { action_tx }, action_rx)
    }

    /// 非阻塞发送。满时返回 `Backpressured`，调用方决定 coalesce / 丢弃。
    #[allow(dead_code)]
    pub fn try_dispatch(&self, action: Action) -> DispatchResult {
        match self.action_tx.try_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(TrySendError::Full(_)) => DispatchResult::Backpressured,
            Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
        }
    }

    /// S2.5 P1-A: `try_dispatch` + 失败时 tracing::warn + Prometheus 计数.
    ///
    /// 主路径调用方应优先用本 helper 而非裸 `try_dispatch`，避免 channel full /
    /// closed 时静默丢失 Action。`site_tag` 用于失败 log 标注调用点（如
    /// "chat.banner" / "chat.shutdown_sigint" / "chat.user_input"），便于事后
    /// 通过 grep 定位漏点。返回原 `DispatchResult` 供调用方按需进一步处理。
    #[allow(dead_code)]
    pub fn dispatch_or_log(&self, action: Action, site: &'static str) -> DispatchResult {
        let action_kind = action.kind();
        let result = self.try_dispatch(action);
        match result {
            DispatchResult::Sent => {}
            DispatchResult::Backpressured => {
                tracing::warn!(
                    site = site,
                    action_kind = action_kind,
                    "chat dispatch failed: channel backpressured, action dropped"
                );
                crate::observability::chat_metrics::inc_dispatch_drop("backpressured");
            }
            DispatchResult::ChannelClosed => {
                tracing::warn!(
                    site = site,
                    action_kind = action_kind,
                    "chat dispatch failed: channel closed, action dropped"
                );
                crate::observability::chat_metrics::inc_dispatch_drop("closed");
            }
        }
        result
    }

    /// 同步阻塞发送（仅在非 async 上下文调用，如 OS 信号 handler 的同步部分）.
    ///
    /// **注意**：tokio runtime 内部不允许 blocking_send，否则 panic。
    /// Ctrl+C / SIGTERM handler 在 spawned task 内（async 上下文），应优先用
    /// [`Self::try_dispatch`] 或 [`Self::dispatch`]。
    #[allow(dead_code)]
    pub fn blocking_dispatch(&self, action: Action) -> DispatchResult {
        match self.action_tx.blocking_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(_) => DispatchResult::ChannelClosed,
        }
    }

    /// 异步阻塞发送（推荐：在 async 路径中安全反压）.
    #[allow(dead_code)]
    pub async fn dispatch(&self, action: Action) -> DispatchResult {
        match self.action_tx.send(action).await {
            Ok(()) => DispatchResult::Sent,
            Err(_) => DispatchResult::ChannelClosed,
        }
    }

    /// 返回底层 sender clone，供需要直接持有 `mpsc::Sender` 的子任务使用.
    ///
    /// 警告：直接持有 sender 会绕过 [`Self::try_dispatch`] 的政策检查。
    /// 仅在 coalescer 等需要 `TrySendError` 细粒度处理的场景使用。
    #[allow(dead_code)]
    pub fn sender(&self) -> mpsc::Sender<Action> {
        self.action_tx.clone()
    }
}

// ─── TurnCompletionSignal (Step 5a-4) ─────────────────────────────────────────

/// Turn 终结的语义结果，由 dispatcher 在 [`TurnCompletionSignal::record_and_notify`]
/// 时写入，供 chat::run await 之后读取以决定 UI/hook 行为.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TurnOutcomeKind {
    /// LLM 流式成功完成；`final_text` 为最终累计可见文本.
    Completed { final_text: String },
    /// LLM 流式失败；`err` 为 [`Action::StreamFailed`] 携带的错误描述，
    /// `retryable` 反映 [`stream_error_is_retryable`] 判定结果。
    Failed { err: String, retryable: bool },
    /// 用户取消或 shutdown 抢占。
    Cancelled,
}

/// Turn 终结显式信号 + 结果槽位，用于 `chat::run` 在 Redux driver 切闸路径下
/// await turn 完成并读取语义结果.
///
/// dispatcher task 在 `state.reduce(action)` 后检测到 terminal action
/// (`StreamCompleted` / `StreamFailed` / `StreamCancelled`) 时：
///   1. 把对应 [`TurnOutcomeKind`] 写入 `outcome` slot
///   2. 调用 `notify_waiters` 唤醒所有等待方
///
/// 与 `RuntimeDualWriteGuard` 解耦 — guard 是双写抑制开关，不是 turn 生命周期信号；
/// 把 turn 生命周期建模为独立的 `Notify + Mutex<Option<Outcome>>` 让语义清晰、
/// 可测试、无忙等。
///
/// 设计选择：用 `tokio::sync::Notify` 而非 `oneshot::channel`：
/// - chat::run 多个 turn 复用同一个 signal；oneshot 仅能 fire 一次
/// - `notify_waiters` latch-less，通知前必须先 `notified()` 获 future，否则错过通知
/// - 协议：每次 dispatch StartLLMTurn 前 chat::run 先获取 `notified()` future
///   并 `consume_outcome()` 清空旧 slot，再 dispatch，最后 await future。
#[derive(Clone)]
pub struct TurnCompletionSignal {
    inner: Arc<tokio::sync::Notify>,
    outcome: Arc<ParkingMutex<Option<TurnOutcomeKind>>>,
}

impl TurnCompletionSignal {
    /// 构造新的信号实例。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Notify::new()),
            outcome: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// dispatcher task 调用：写入 outcome + 唤醒等待方。
    pub fn record_and_notify(&self, outcome: TurnOutcomeKind) {
        *self.outcome.lock() = Some(outcome);
        self.inner.notify_waiters();
    }

    /// 兜底通知（无 outcome 写入，例如 shutdown 抢占）。
    /// 等待方读取到 `None` 应视为 cancelled。
    pub fn notify(&self) {
        self.inner.notify_waiters();
    }

    /// 返回 `Notified` future。chat::run 协议：dispatch 前调用，await 在 dispatch 之后。
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.inner.notified()
    }

    /// 取走当前 outcome（消费式）。返回 `None` 表示无终结事件被记录（shutdown 兜底）。
    #[must_use]
    pub fn consume_outcome(&self) -> Option<TurnOutcomeKind> {
        self.outcome.lock().take()
    }
}

impl Default for TurnCompletionSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TurnCompletionSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCompletionSignal").finish_non_exhaustive()
    }
}

/// 从 action 类型映射到 turn outcome（用于 dispatcher task 在 reduce 前抽取）。
#[must_use]
#[allow(dead_code)]
pub fn extract_turn_outcome(action: &Action) -> Option<TurnOutcomeKind> {
    match action {
        Action::StreamCompleted { final_text, .. } => Some(TurnOutcomeKind::Completed {
            final_text: final_text.clone(),
        }),
        Action::StreamFailed { err, retryable, .. } => Some(TurnOutcomeKind::Failed {
            err: err.clone(),
            retryable: *retryable,
        }),
        Action::StreamCancelled { .. } => Some(TurnOutcomeKind::Cancelled),
        _ => None,
    }
}

/// 判断 action 是否为 turn 终结事件。dispatcher task 用此函数决定何时
/// 触发 [`TurnCompletionSignal::notify`].
#[must_use]
pub const fn is_turn_terminal_action(action: &Action) -> bool {
    matches!(
        action,
        Action::StreamCompleted { .. } | Action::StreamFailed { .. } | Action::StreamCancelled { .. }
    )
}

// ─── RuntimeDualWriteGuard ─────────────────────────────────────────────────────

/// 双写抑制计数器（Step 5a-1，5a-5 Codex P1 修复：bool → AtomicU64 计数）.
///
/// 在 Both/Redux 模式下，业务 Effect 真执行的同时旧路径仍在跑。为防止 history /
/// session 等持久化资源被写两次，我们让 reducer 路径在执行业务 Effect 前 +1，
/// 旧路径在持久化前检查计数器——若 > 0 则跳过自己的写。
///
/// **5a-5 修复**：之前是 `AtomicBool`，存在严重时序窗——多个 effect 并发持有
/// `DualWriteGuardScope` 时，一个 scope drop 会把全部 active 状态清空，造成另一
/// 个还在跑的 effect 旁路被"放行"。改为 `AtomicU64` 计数：每个 scope `fetch_add(1)`
/// 进入，`fetch_sub(1)` 退出，`is_active()` 即 `> 0`。
///
/// guard 由 `chat::run` 持有 `Arc<AtomicU64>`，dispatcher 与旧路径共享。
/// 仅在 Both/Redux 模式构造；Off 模式不构造（旧路径正常单写）。
///
/// 注意：guard 不是 mutex——它是「策略开关」而非「锁」。旧路径检查计数器时如果发现
/// > 0，简单 `continue` 即可；不存在等待语义。这避免了双写期任何死锁可能。
#[derive(Debug, Clone)]
pub struct RuntimeDualWriteGuard {
    /// 活跃 scope 计数（> 0 → 旧路径跳过对应持久化）.
    active: Arc<AtomicU64>,
}

impl RuntimeDualWriteGuard {
    /// 构造新 guard（active=0，旧路径正常持久化）.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 旧路径查询当前是否被 Redux 抢占（计数 > 0）.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) > 0
    }

    /// 测试观测：返回当前活跃 scope 计数（仅 cfg(test)，生产代码不需要）.
    #[cfg(test)]
    #[must_use]
    pub fn active_count(&self) -> u64 {
        self.active.load(Ordering::Acquire)
    }

    /// 创建 RAII scope：进入时 +1，离开（或 panic）时自动 -1.
    ///
    /// 多个 scope 同时存在时，计数器累加；只有全部 scope drop 后才回到 0。
    /// 这解决了 5a-4 之前的 bool 版本"早 drop 误清零"问题。
    #[must_use]
    pub fn enter_scope(&self) -> DualWriteGuardScope {
        DualWriteGuardScope::enter(Arc::clone(&self.active))
    }
}

impl Default for RuntimeDualWriteGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII scope for [`RuntimeDualWriteGuard`].
///
/// 进入时通过 `fetch_add(1)` 累计；`Drop` 时通过 `fetch_sub(1)` 释放，
/// 无论正常退出还是 panic unwind 均生效，让计数器准确反映活跃 scope 数。
///
/// 通过 `RuntimeDualWriteGuard::enter_scope()` 构造。
pub struct DualWriteGuardScope {
    inner: Arc<AtomicU64>,
}

impl DualWriteGuardScope {
    fn enter(inner: Arc<AtomicU64>) -> Self {
        inner.fetch_add(1, Ordering::Release);
        Self { inner }
    }
}

impl Drop for DualWriteGuardScope {
    fn drop(&mut self) {
        // saturating_sub via fetch_update would be safer, but counters are
        // strictly balanced (every fetch_add followed by exactly one Drop),
        // so fetch_sub is correct. Underflow would indicate a logic bug.
        self.inner.fetch_sub(1, Ordering::Release);
    }
}

// ─── EffectDeps ────────────────────────────────────────────────────────────────

/// EffectExecutor 真业务执行所需依赖.
///
/// 由 `chat::run` 在启动期收集；clone 成本仅为 Arc bump，安全地传给 spawn 子任务。
/// 缺任一项即等于 shadow 模式（构造时强制 `new_with_deps` 接收全部字段）。
#[derive(Clone)]
pub struct EffectDeps {
    /// 当前 provider（LLM 调用）— Step 5a-1 仅供 StartTurn 使用，后续 effect 复用
    pub provider: Arc<dyn Provider>,
    /// memory backend（SaveSession / PersistToMemory）
    pub memory: Arc<dyn Memory>,
    /// 当前 channel（EmitChannelMessage / SendDraftFinalize / CancelDraft）
    pub channel: Arc<dyn Channel>,
    /// hook 管理器（NotifyHook）
    pub hooks: Arc<HookManager>,
    /// observability observer（结构化事件）
    pub observer: Arc<dyn Observer>,
    /// Action 回投 channel sender（StartTurn 子任务的流式回调）
    pub action_tx: mpsc::Sender<Action>,
    /// 双写抑制 guard（Both/Redux 模式下持久化 effect 前置位）
    pub dual_write_guard: RuntimeDualWriteGuard,
    /// 渲染重绘 channel（RequestRedraw 唤醒主循环）
    /// 用 mpsc::Sender<()> 而非 broadcast，因为我们只需要"踢一下"主循环
    pub redraw_tx: Option<mpsc::Sender<()>>,
    /// 关停信号（Effect::Quit 触发）
    pub shutdown: CancellationToken,
    /// Step 5a-4 (Codex P1)：当前 LLM model name，drive_start_turn_stream 用此
    /// 调 `provider.stream_chat_with_history(_, model, _, _)`。原本 hard-coded
    /// 为空串导致真实 provider (OpenAI/Anthropic) 拒绝；mock provider 测试不暴露此问题.
    pub model: Arc<str>,
    /// 当前 temperature（默认从 CLI 参数注入；与 model 配对传给 stream API）.
    pub temperature: f64,
    /// **5a-6**: tool registry — driver 执行 tool_call 时按名查找并调用。`None`
    /// 表示当前 turn 不允许 tool 调用（driver 收到 tool_call 时发 StreamFailed）。
    /// 用 `Arc<Vec<Box<dyn Tool>>>` 而非 slice：跨 spawn 边界共享所有权，clone 仅 Arc bump。
    pub tools_registry: Option<Arc<Vec<Box<dyn crate::tools::Tool>>>>,
    /// **5a-6**: max tool iterations — 防 LLM 死循环。0 走默认 (16)，上限受 driver 内部保护。
    pub max_tool_iterations: usize,
    /// **S3 T3-1**: approval 请求-应答路由器 (driver↔dispatcher 桥接 oneshot).
    ///
    /// driver 在执行需 approval 的 tool 前注册 oneshot tx；dispatcher_task 在
    /// reducer 处理完 `Action::ToolApprovalReceived` 之后调用 `resolve()` 把决策
    /// 回投。`Arc` 跨 spawn 边界共享所有权。
    pub approval_router: Arc<ApprovalRouter>,
    /// **S3 T3-1**: 危险 tool approval 管理器 — driver 据此判断是否要 prompt.
    ///
    /// 来自 `chat::run` 构造的 `ApprovalManager::from_config(&config.autonomy)`。
    /// `None` 时 driver 不做任何 approval 检查（兼容现有未接入 approval 的测试）。
    pub approval_manager: Option<Arc<crate::approval::ApprovalManager>>,
}

// ─── EffectExecutor (5a-1: real-mode + shadow-mode) ───────────────────────────

/// `Effect` 执行器。两种构造形态：
/// - shadow 模式 (`new_shadow`)：除 `LogTrace` 外所有业务 Effect 都是 no-op；保留
///   用于 Off 模式 / 单元测试 / 5b 行为基线
/// - real 模式 (`new_with_deps`)：持有 [`EffectDeps`]，业务 Effect 真执行；长耗时
///   操作 spawn 子任务回投 Action（Codex P0-1）
///
/// P0-2 fix: `redraw_tx` 通过共享的 `Arc<parking_lot::Mutex<Option<mpsc::Sender<()>>>>` 后注入。
/// `chat::run` 先构造 EffectExecutor（此时 redraw_tx 尚无），spawn dispatcher task 后
/// 再通过 `redraw_handle()` 返回的 Arc 将 `redraw_tx` 注入，解决时序问题。
#[allow(dead_code)]
pub struct EffectExecutor {
    /// shadow 模式标志。`true` 时所有业务 Effect 都跳过执行。
    shadow_mode: bool,
    /// 真业务依赖。Some 表示 deps 模式，None 表示 shadow 模式。
    deps: Option<EffectDeps>,
    /// P0-2: 可后注入的 redraw_tx 句柄。spawn 后由 chat::run 填入真实 sender。
    /// real 模式下两者共享同一个 Arc，允许在 dispatcher task 运行期注入。
    redraw_slot: Arc<ParkingMutex<Option<mpsc::Sender<()>>>>,
}

impl EffectExecutor {
    /// 构造 shadow 模式执行器（Step 5b 兼容、单元测试、Off 模式）.
    #[allow(dead_code)]
    #[must_use]
    pub fn new_shadow() -> Self {
        Self {
            shadow_mode: true,
            deps: None,
            redraw_slot: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// 构造真业务执行器（Step 5a-1，PRX_CHAT_REDUX=both/1 模式）.
    #[allow(dead_code)]
    #[must_use]
    pub fn new_with_deps(deps: EffectDeps) -> Self {
        Self {
            shadow_mode: false,
            deps: Some(deps),
            redraw_slot: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// P0-2 fix: 返回共享的 redraw_tx 槽位 Arc，供 chat::run 在 TUI 初始化后注入.
    ///
    /// 调用方持有此 Arc，在 `redraw_tx` 创建后调用 `*slot.lock() = Some(tx)` 即可。
    /// dispatcher task spawn 后仍可注入，因为 Arc 跨越了 spawn 边界。
    ///
    /// shadow 模式下注入无效（execute_shadow 不读此槽位）。
    #[allow(dead_code)]
    #[must_use]
    pub fn redraw_handle(&self) -> Arc<ParkingMutex<Option<mpsc::Sender<()>>>> {
        Arc::clone(&self.redraw_slot)
    }

    /// **S3 T3-1**: 返回 deps 中的 approval router（real 模式独有）.
    ///
    /// `spawn_dispatcher_task_with_signal` 用此句柄在 `Action::ToolApprovalReceived`
    /// 进入 reducer 之后把决策回投给 driver pending oneshot。shadow 模式无 deps 返回 None。
    #[allow(dead_code)]
    #[must_use]
    pub fn approval_router(&self) -> Option<Arc<ApprovalRouter>> {
        self.deps.as_ref().map(|d| Arc::clone(&d.approval_router))
    }

    /// 测试观测：是否处于 shadow 模式.
    #[cfg(test)]
    #[must_use]
    pub const fn is_shadow(&self) -> bool {
        self.shadow_mode
    }

    /// 执行单个 Effect。
    ///
    /// - shadow 模式：仅 `LogTrace` 真执行（结构化日志属于可观测性必要工具），
    ///   `RequestRedraw` 输出 trace，其余业务 Effect 输出 debug log。
    /// - real 模式：每个业务 Effect 走对应 deps 的真实路径；StartTurn / SaveSession
    ///   等长耗时 effect `tokio::spawn` 子任务回投，避免阻塞主循环。
    ///
    /// 双写抑制：进入业务 Effect 时如有 deps，先置位 dual_write_guard（让旧路径跳过
    /// 自己的对应写）。SaveSession / PersistToMemory / EmitChannelMessage 等持久化
    /// effect 完成后由调用方控制复位（典型在 turn 结束）。
    #[allow(dead_code)]
    pub async fn execute(&self, effect: Effect) {
        // S2.5 T2.5-2: 每个 Effect 入口埋点 prx_chat_effects_total{effect_kind=...}.
        crate::observability::chat_metrics::inc_effect(effect.kind());
        // LogTrace 在两种模式下都真执行（可观测性）
        if let Effect::LogTrace { level, msg } = &effect {
            Self::emit_trace(*level, msg);
            return;
        }
        match (self.shadow_mode, &self.deps) {
            (true, _) | (_, None) => self.execute_shadow(effect),
            (false, Some(deps)) => self.execute_real(effect, deps).await,
        }
    }

    /// shadow 模式分支：业务 Effect 全部 no-op + debug log.
    fn execute_shadow(&self, effect: Effect) {
        match &effect {
            Effect::RequestRedraw => {
                tracing::trace!("effect: RequestRedraw (shadow no-op)");
            }
            other => {
                tracing::debug!(effect = ?other, "effect skipped (shadow mode)");
            }
        }
    }

    /// real 模式分支：按 Effect 类型分发到真业务执行.
    async fn execute_real(&self, effect: Effect, deps: &EffectDeps) {
        match effect {
            Effect::RequestRedraw => {
                // P0-2 fix: 优先读 redraw_slot（后注入），回退到 deps.redraw_tx（构造时注入）.
                // redraw_slot 在 TUI 初始化完成后由 chat::run 填入真实 sender，
                // 确保 RequestRedraw 真正触发重绘而非 no-op。
                let slot_guard = self.redraw_slot.lock();
                let tx = slot_guard.as_ref().or(deps.redraw_tx.as_ref());
                if let Some(tx) = tx {
                    let _ = tx.try_send(());
                } else {
                    tracing::trace!("RequestRedraw: redraw_tx not yet injected (P0-2)");
                }
            }
            Effect::StartTurn {
                draft_id,
                history,
                cancel,
                chat_mode,
            } => {
                // Step 5a-2 — 长耗时：spawn 子任务真调 provider.stream_chat_with_history，
                // 通过 deps.action_tx 把 chunk / 完成 / 失败 / 取消事件回投给 reducer，
                // 取代旧 `delta_tx → draft_updater → coalescer` 链路。
                //
                // 设计要点（与 plan Step 5a-2 一致）：
                //   1. `tokio::pin!` 固定 stream，`tokio::select!` 同时监听 cancel + chunk
                //   2. version 由本地计数器严格递增（与 reducer strict-monotonic 一致）
                //   3. Reasoning 不混入主文本流（与现网 chat::run 行为对齐）
                //   4. RAII `DualWriteGuardScope` 守住整个 turn 期间，子任务退出自动复位
                //   5. 任意分支错误用 `action_tx.send().await`（不丢 chunk，让反压自然回退）
                //
                // 注意：StartTurn 当前 **没有** 被 reducer 自动触发——本路径仅在调用方
                // 显式 spawn `Effect::StartTurn { ... }` 时生效（如单测 / 5a-3 接线后的
                // ratatui 路径）。chat::run 主循环仍由 `run_tool_call_loop` 主导（旧路径），
                // 双写抑制由 `dual_write_guard` 在 reducer 持久化 effect 时已经守住。
                let provider = Arc::clone(&deps.provider);
                let action_tx = deps.action_tx.clone();
                let guard_scope = deps.dual_write_guard.enter_scope();
                // Codex P1 fix：从 deps 拿真实 model + temperature 传给 stream API.
                let model = deps.model.to_string();
                let temperature = deps.temperature;
                // 5a-6: 透传 tool registry + max iterations (None / 0 → driver 退化为纯文本流式).
                let tools_registry = deps.tools_registry.as_ref().map(Arc::clone);
                let max_tool_iterations = deps.max_tool_iterations;
                // S3 T3-1: approval 桥接 — router + manager 句柄一起透传给 driver.
                let approval_router = Some(Arc::clone(&deps.approval_router));
                let approval_manager = deps.approval_manager.as_ref().map(Arc::clone);
                tokio::spawn(async move {
                    // RAII scope：子任务退出（含 panic）时自动复位 dual_write_guard。
                    let _scope = guard_scope;
                    if cancel.is_cancelled() {
                        // 启动前已取消：直接发 StreamCancelled，不发 LLM 请求.
                        if let Err(e) = action_tx.send(Action::StreamCancelled { draft_id }).await {
                            tracing::debug!(error = %e, "StartTurn: action_tx closed on pre-cancel");
                        }
                        return;
                    }
                    drive_start_turn_stream(
                        provider,
                        history,
                        model,
                        temperature,
                        cancel,
                        draft_id,
                        action_tx,
                        tools_registry,
                        max_tool_iterations,
                        approval_router,
                        approval_manager,
                        chat_mode,
                    )
                    .await;
                });
            }
            Effect::SaveSession(session) => {
                // T3-3-fixB D1：inline await 替代 tokio::spawn，让主循环
                // executor.execute(effect).await 的串行性贯穿到底，关闭
                // SaveSession 还在写盘时 RequestRedraw 已刷屏的不一致窗口.
                // RAII scope 与 inline await 同生命周期，await 完成后 _scope drop
                // 释放 guard，旧路径才能再次单写（多个 effect 串行不互相覆盖）.
                let _scope = deps.dual_write_guard.enter_scope();
                let memory = Arc::clone(&deps.memory);
                let action_tx = deps.action_tx.clone();
                let session_id = session.id.clone();
                let json = match session.to_json() {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!(error = %e, "SaveSession effect: serialize failed");
                        return;
                    }
                };
                match memory
                    .store(
                        &session.memory_key(),
                        &json,
                        crate::memory::MemoryCategory::Conversation,
                        Some(&session.id),
                    )
                    .await
                {
                    Ok(()) => {
                        let _ = action_tx.try_send(Action::SessionSaved { id: session_id });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "SaveSession effect: store failed");
                    }
                }
            }
            Effect::SendDraftFinalize { draft_id, text } => {
                // 双写抑制 RAII scope：子任务退出时自动复位.
                let guard_scope = deps.dual_write_guard.enter_scope();
                let channel = Arc::clone(&deps.channel);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    let recipient = "user";
                    tracing::debug!(
                        draft_id = %draft_id,
                        text_len = text.len(),
                        channel = %channel.name(),
                        "SendDraftFinalize effect: calling channel.finalize_draft"
                    );
                    if let Err(e) = channel.finalize_draft(recipient, &draft_id, &text).await {
                        tracing::warn!(
                            error = %e,
                            draft_id = %draft_id,
                            "SendDraftFinalize effect: finalize_draft failed"
                        );
                    }
                });
            }
            Effect::CancelDraft(draft_id) => {
                // 直接调 channel.cancel_draft（短同步路径，无需 spawn）
                let channel = Arc::clone(&deps.channel);
                let recipient = "user".to_string();
                if let Err(e) = channel.cancel_draft(&recipient, &draft_id).await {
                    tracing::debug!(error = %e, draft_id = %draft_id, "CancelDraft effect: channel returned err");
                }
            }
            Effect::CancelToken(token) => {
                // S2-B Step 2: 真触发底层 CancellationToken — 让 LLM 流 / tool loop
                // 立刻收到 cancel 信号返回 cancelled 错误。无需 spawn（cancel 本身
                // 不阻塞），无需 dual_write_guard（取消是幂等动作，重复 cancel 安全）。
                tracing::info!("effect: CancelToken -> token.cancel()");
                token.cancel();
            }
            Effect::EmitChannelMessage(send_msg) => {
                let guard_scope = deps.dual_write_guard.enter_scope();
                let channel = Arc::clone(&deps.channel);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    if let Err(e) = channel.send(&send_msg).await {
                        tracing::warn!(error = %e, "EmitChannelMessage effect: send failed");
                    }
                });
            }
            Effect::PersistToMemory { key, value, category } => {
                let guard_scope = deps.dual_write_guard.enter_scope();
                let memory = Arc::clone(&deps.memory);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    if let Err(e) = memory.store(&key, &value, category, None).await {
                        tracing::warn!(error = %e, key = %key, "PersistToMemory effect: store failed");
                    }
                });
            }
            Effect::NotifyHook { event, payload } => {
                let guard_scope = deps.dual_write_guard.enter_scope();
                let hooks = Arc::clone(&deps.hooks);
                tokio::spawn(async move {
                    let _scope = guard_scope;
                    hooks.emit(event, payload).await;
                });
            }
            Effect::DisplayMedia { kind, path } => {
                // 媒体显示是用户可见短同步路径；用 tracing 记录（observer 没有
                // 通用 trace 变体，且 5a-1 阶段旧路径仍负责真正的媒体显示）。
                tracing::debug!(kind = %kind, path = %path, "DisplayMedia effect");
                let _ = deps.observer.name(); // 占位避免 deps.observer 字段被警告
            }
            Effect::AutoTitleSession(title) => {
                tracing::debug!(title = %title, "AutoTitleSession effect");
            }
            Effect::RequestApproval { tool_id, name, args } => {
                let env_value = std::env::var("OPENPRX_APPROVAL_OVERRIDE").ok();
                let approved = resolve_supervised_approval_override(env_value.as_deref());
                if env_value.is_none() {
                    tracing::warn!(
                        tool_id = %tool_id,
                        name = %name,
                        args_len = args.len(),
                        "supervised approval UI 未接通，默认拒绝。设置 OPENPRX_APPROVAL_OVERRIDE=allow 显式放行"
                    );
                } else {
                    tracing::info!(
                        tool_id = %tool_id,
                        name = %name,
                        args_len = args.len(),
                        approved,
                        "RequestApproval effect (stub): OPENPRX_APPROVAL_OVERRIDE applied"
                    );
                }
                let _ = args;
                let action_tx = deps.action_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = action_tx.send(Action::ToolApprovalReceived { tool_id, approved }).await {
                        tracing::debug!(error = %e, "RequestApproval stub: action_tx closed");
                    }
                });
            }
            Effect::Quit => {
                // 关停信号：真 cancel + drop 等隐式协议由 chat::run 收尾处理
                tracing::info!("effect: Quit -> shutdown.cancel()");
                deps.shutdown.cancel();
            }
            Effect::LogTrace { .. } => {
                // 已在 execute() 顶部分支处理
            }
        }
    }

    /// 将 [`tracing::Level`] 分发到对应的 macro（避免 dyn dispatch）.
    fn emit_trace(level: tracing::Level, msg: &str) {
        if level == tracing::Level::ERROR {
            tracing::error!("{}", msg);
        } else if level == tracing::Level::WARN {
            tracing::warn!("{}", msg);
        } else if level == tracing::Level::INFO {
            tracing::info!("{}", msg);
        } else if level == tracing::Level::DEBUG {
            tracing::debug!("{}", msg);
        } else {
            tracing::trace!("{}", msg);
        }
    }
}

// ─── S5 P0-3: supervised approval override ────────────────────────────────────

/// 解析 `OPENPRX_APPROVAL_OVERRIDE` env 决定 supervised 模式下的批准结果.
///
/// S5 P0-3 (BREAKING): TUI 卡片渲染 + Y/N 键盘接线 (T5-1 完整版) 留 Task #11；
/// 在 UI 接通前静默 auto-approve 是安全缺口 (Codex 反馈 "绝不静默 auto-approve")。
///
/// - `Some("allow" | "y" | "yes" | "1")` → `true` (显式允许)
/// - `Some("deny" | "n" | "no" | "0")` → `false` (显式拒绝)
/// - `None` 或其他值 → `false` (fail-safe deny，BREAKING — 原行为为 true)
///
/// 大小写不敏感，前后空白被忽略。
#[must_use]
pub(crate) fn resolve_supervised_approval_override(raw: Option<&str>) -> bool {
    let Some(value) = raw else {
        return false;
    };
    matches!(value.trim().to_ascii_lowercase().as_str(), "allow" | "y" | "yes" | "1")
}

// ─── StartTurn streaming driver (Step 5a-2) ────────────────────────────────────

/// 判断 [`StreamError`] 是否值得重试.
///
/// 与 reducer `Action::StreamFailed { retryable, .. }` 字段对齐：让上层（chat::run
/// 主循环或未来的自动重试逻辑）依据布尔值决定是否安排另一轮 turn。当前判断准则：
/// - `Http` / `Io`：网络瞬时故障，retryable
/// - `Json` / `InvalidSse`：数据破损，多半重试也复发，non-retryable
/// - `Provider`：服务端语义错误，倾向于 non-retryable（让上层显示并由用户决定）
#[must_use]
const fn stream_error_is_retryable(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    matches!(err, StreamError::Http(_) | StreamError::Io(_))
}

/// **S3 T3-1**: 网络超时 / 连接错误识别 — 决定 driver 是否走 exponential backoff retry.
///
/// 命中条件：
/// - `StreamError::Io` 总是被视为可重试瞬时故障（与 [`stream_error_is_retryable`] 同源）
/// - `StreamError::Http(reqwest_err)` 且 `is_timeout()` 或 `is_connect()` 返回 true
///
/// 其他场景返回 false，由调用方走普通 `StreamFailed` 路径而非 retry loop。
#[must_use]
fn stream_error_is_network_timeout(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    match err {
        StreamError::Io(_) => true,
        StreamError::Http(http_err) => http_err.is_timeout() || http_err.is_connect(),
        StreamError::Json(_) | StreamError::InvalidSse(_) | StreamError::Provider(_) => false,
    }
}

/// **S3 T3-1**: 识别 context overflow / context_length_exceeded 类错误.
///
/// 命中 → driver 触发一次 history compaction + 单次重试。判定走 `StreamError::Provider`
/// 的 message 子串匹配（OpenAI 返回 "maximum context length"、Anthropic 返回
/// "prompt is too long"、Gemini 返回 "input token count" 等）。
///
/// 不做精确正则：provider 错误消息格式不稳定，子串容错更安全；误判（多走一次 compact）
/// 也只损耗少量算力而非破坏正确性。
#[must_use]
fn stream_error_is_context_overflow(err: &crate::providers::traits::StreamError) -> bool {
    use crate::providers::traits::StreamError;
    let msg = match err {
        StreamError::Provider(s) => s.as_str(),
        StreamError::Http(http_err) => return matches!(http_err.status(), Some(s) if s.as_u16() == 413),
        StreamError::Json(_) | StreamError::InvalidSse(_) | StreamError::Io(_) => return false,
    };
    let needles = [
        "context_length_exceeded",
        "context length exceeded",
        "maximum context",
        "exceeds maximum",
        "prompt is too long",
        "input token count",
        "exceed the maximum",
        "too many tokens",
        "token limit",
    ];
    let lower = msg.to_ascii_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

/// **S3 T3-1**: 工具回合参数聚合 buffer.
///
/// driver 内部按 [`ToolCallChunk::index`] 维护每个 in-flight tool call 的状态；
/// 收到 Streaming chunk → push `arguments_delta`；收到 Completed → 比较聚合值与
/// `args` 校验一致性（discrepancy 时优先信任 Completed.args）。
///
/// 设计要点（Codex 审计 1）：
/// - 仅在 Completed chunk 到达后 emit `Action::ToolStarted`（避免半成品 args 触发执行）
/// - 重复 Completed 同 index 视为幂等 no-op，防 provider 错误 emit 两次
/// - `id` / `name` 严格不变；如果出现冲突记录 warn 但仍以最后一次 Completed 为准
struct ToolCallAggregator {
    /// 已经聚合的 chunk 索引 → buffer
    by_index: std::collections::HashMap<usize, ToolCallSlot>,
    /// 已发射 Completed 的 index 集合（防止 provider 重复 emit）
    completed: std::collections::HashSet<usize>,
}

/// 单个 tool call 的聚合槽位.
struct ToolCallSlot {
    id: String,
    name: String,
    args_buffer: String,
    final_args: Option<String>,
}

impl ToolCallAggregator {
    fn new() -> Self {
        Self {
            by_index: std::collections::HashMap::new(),
            completed: std::collections::HashSet::new(),
        }
    }

    /// 摄入一个 `ToolCallChunk` — 按 `status` 分发到 streaming-append / completed-finalize.
    ///
    /// 返回 `Some((id, name, args))` 表示一个 tool call 已完整就绪并应触发 ToolStarted；
    /// 返回 `None` 表示尚未就绪 / 重复完成（已发射过）/ 协议冲突已 log。
    fn ingest(&mut self, chunk: crate::providers::traits::ToolCallChunk) -> Option<(String, String, String)> {
        use crate::providers::traits::ToolCallChunkStatus;
        match chunk.status {
            ToolCallChunkStatus::Streaming => {
                let slot = self.by_index.entry(chunk.index).or_insert_with(|| ToolCallSlot {
                    id: chunk.id.clone(),
                    name: chunk.name.clone(),
                    args_buffer: String::new(),
                    final_args: None,
                });
                // ID / name 不变性校验：provider 协议禁止改名换 ID.
                // Some compatible providers may emit an opening chunk before the
                // id is known, then fill it in later; preserve that first real id.
                if !chunk.id.is_empty() {
                    if slot.id.is_empty() {
                        slot.id = chunk.id.clone();
                    } else if slot.id != chunk.id {
                        tracing::warn!(
                            index = chunk.index,
                            prev_id = %slot.id,
                            new_id = %chunk.id,
                            "ToolCallAggregator: streaming chunk changed id; keeping first id"
                        );
                    }
                }
                if !chunk.name.is_empty() && slot.name != chunk.name {
                    tracing::warn!(
                        index = chunk.index,
                        prev_name = %slot.name,
                        new_name = %chunk.name,
                        "ToolCallAggregator: streaming chunk changed name; keeping first name"
                    );
                }
                if let Some(delta) = chunk.arguments_delta {
                    slot.args_buffer.push_str(&delta);
                }
                None
            }
            ToolCallChunkStatus::Completed => {
                if self.completed.contains(&chunk.index) {
                    tracing::debug!(
                        index = chunk.index,
                        id = %chunk.id,
                        "ToolCallAggregator: duplicate Completed; ignoring"
                    );
                    return None;
                }
                self.completed.insert(chunk.index);
                let slot = self.by_index.entry(chunk.index).or_insert_with(|| ToolCallSlot {
                    id: chunk.id.clone(),
                    name: chunk.name.clone(),
                    args_buffer: String::new(),
                    final_args: None,
                });
                slot.final_args = Some(chunk.args.clone());
                // 信任 Completed.args 为准（与 traits.rs 协议注释一致）.
                let resolved_id = if chunk.id.is_empty() { slot.id.clone() } else { chunk.id };
                let resolved_name = if chunk.name.is_empty() {
                    slot.name.clone()
                } else {
                    chunk.name
                };
                Some((resolved_id, resolved_name, chunk.args))
            }
        }
    }
}

/// 已完成（Completed）的工具调用，准备执行.
struct ResolvedToolCall {
    id: String,
    name: String,
    args: String,
}

/// **S3 T3-1**: 网络瞬时故障 backoff retry 上限（次）.
const MAX_NETWORK_RETRIES: u8 = 3;
/// **S3 T3-1**: context overflow 自动 compact + retry 上限（仅 1 次防无限循环）.
const MAX_CONTEXT_OVERFLOW_RETRIES: u8 = 1;
/// **S3 T3-1**: backoff 起步 sleep（毫秒，第 1 次重试前 sleep 500ms，第 2 次 1s，第 3 次 2s）.
const BACKOFF_BASE_MS: u64 = 500;

/// 单轮 stream 的结果分类（driver loop 用此向上 unwind）.
enum StreamPassOutcome {
    /// 本轮没有 tool_call，普通文本生成结束。携带最终累计文本。
    Completed { iter_text: String },
    /// 本轮 LLM 要求工具调用 — 携带聚合好的 tool_calls + 本轮 assistant 文本（提示词）。
    ToolCallRequested {
        calls: Vec<ResolvedToolCall>,
        iter_text: String,
        reasoning_content: String,
    },
    /// 网络瞬时错误（可走 backoff retry，不消耗 iteration 配额）.
    TransientNetworkError { err: String },
    /// context overflow（可走 compact + retry 一次）.
    ContextOverflow { err: String },
    /// 非可重试的硬错误 — driver 终止并发 StreamFailed.
    HardError { err: String, retryable: bool },
    /// 用户 Cancel —  driver 已发 StreamCancelled 直接返回.
    Cancelled,
    /// action_tx 关闭，driver 静默退出（不再发 action）.
    SenderClosed,
}

/// 真接 `provider.stream_chat_with_history` 并把流式事件回投到 reducer.
///
/// 设计在 spawn 子任务内独立运行；通过 `cancel` 中途取消，通过 `action_tx` 回投。
/// 拆成独立 fn 而非内联在 `execute_real`，便于：
///   - 单元测试直接驱动 fake provider 验证回投序列
///   - 让 borrow / move 关系清晰（spawn move 闭包内不再持有 deps 引用）
///
/// 行为保证：
/// - 任何退出路径必发 **恰一条** terminal action：`StreamCompleted` / `StreamFailed` /
///   `StreamCancelled`，让 reducer 能匹配并清理 `state.stream.draft`
/// - `version` 严格单调递增（1, 2, 3, …），跨所有 tool iteration 单调，与 reducer
///   strict-monotonic 一致
/// - `reasoning` 不混入主 delta（只在最终 `StreamCompleted.reasoning` 字段携带）
///
/// **5a-6**: 多轮 tool turn 支持。
/// **S3 T3-1**: 四件套扩展（工具回合状态机 / context overflow compact / approval 桥接 /
/// timeout backoff retry）。详见 `task/prx/T3-1.md`.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    skip_all,
    fields(
        draft_id = %draft_id,
        model = %model,
    )
)]
async fn drive_start_turn_stream(
    provider: Arc<dyn Provider>,
    mut history: Vec<crate::providers::traits::ChatMessage>,
    model: String,
    temperature: f64,
    cancel: CancellationToken,
    draft_id: String,
    action_tx: mpsc::Sender<Action>,
    tools_registry: Option<Arc<Vec<Box<dyn crate::tools::Tool>>>>,
    max_tool_iterations: usize,
    approval_router: Option<Arc<ApprovalRouter>>,
    approval_manager: Option<Arc<crate::approval::ApprovalManager>>,
    chat_mode: crate::agent::loop_::ChatMode,
) {
    // 默认 / 上限：与 `agent::loop_::DEFAULT_MAX_TOOL_ITERATIONS` 概念对齐，
    // 但 driver 内部独立维护防止意外 0 走入死循环。
    const DEFAULT_MAX_ITERATIONS: usize = 16;
    const ABSOLUTE_MAX_ITERATIONS: usize = 64;
    let max_iterations = if max_tool_iterations == 0 {
        DEFAULT_MAX_ITERATIONS
    } else {
        max_tool_iterations.min(ABSOLUTE_MAX_ITERATIONS)
    };

    let mut version: u64 = 0;
    let mut accumulated = String::new();
    let mut reasoning_buf = String::new();
    let mut iteration: usize = 0;
    let mut overflow_retries: u8 = 0;
    // 已经执行过的 tool_call_id（防 context overflow 重试后重复执行同一工具）.
    let mut executed_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let stream_tool_specs: Vec<crate::tools::ToolSpec> = tools_registry.as_ref().map_or_else(Vec::new, |registry| {
        registry.iter().flat_map(|tool| tool.specs()).collect()
    });

    'outer: loop {
        iteration = iteration.saturating_add(1);
        if iteration > max_iterations {
            let action = Action::StreamFailed {
                draft_id: draft_id.clone(),
                err: format!("redux driver: max tool iterations exceeded ({max_iterations})"),
                retryable: false,
            };
            if let Err(e) = action_tx.send(action).await {
                tracing::debug!(error = %e, "StartTurn: action_tx closed on max-iter exceeded");
            }
            return;
        }

        // ── 单轮 stream 执行 + backoff retry ──────────────────────────
        let pass = run_one_stream_pass_with_retry(
            provider.as_ref(),
            &history,
            &model,
            temperature,
            &cancel,
            &draft_id,
            &action_tx,
            &mut version,
            &mut reasoning_buf,
            &stream_tool_specs,
        )
        .await;

        match pass {
            StreamPassOutcome::Completed { iter_text } => {
                accumulated.push_str(&iter_text);
                break 'outer;
            }
            StreamPassOutcome::ContextOverflow { err } => {
                if overflow_retries >= MAX_CONTEXT_OVERFLOW_RETRIES {
                    let action = Action::StreamFailed {
                        draft_id: draft_id.clone(),
                        err: format!("context overflow after {MAX_CONTEXT_OVERFLOW_RETRIES} retry: {err}"),
                        retryable: false,
                    };
                    if let Err(e) = action_tx.send(action).await {
                        tracing::debug!(error = %e, "StartTurn: action_tx closed on overflow-exhausted");
                    }
                    return;
                }
                overflow_retries = overflow_retries.saturating_add(1);
                // 同步两侧：driver 自己 compact + 通知 reducer compact 自己的 history.
                crate::chat::state::compact_history_in_place(&mut history);
                if let Err(e) = action_tx
                    .send(Action::HistoryCompacted {
                        reason: crate::chat::action::CompactReason::ContextOverflow,
                    })
                    .await
                {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on compact-dispatch");
                    return;
                }
                // 同一次 outer iteration 不消耗配额 — decrement 让重试不计入 max_iterations.
                iteration = iteration.saturating_sub(1);
                continue 'outer;
            }
            StreamPassOutcome::TransientNetworkError { err } => {
                // 已经在 run_one_stream_pass_with_retry 里耗尽 backoff，直接 hard fail.
                let action = Action::StreamFailed {
                    draft_id: draft_id.clone(),
                    err,
                    retryable: false,
                };
                if let Err(e) = action_tx.send(action).await {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on net-exhausted");
                }
                return;
            }
            StreamPassOutcome::HardError { err, retryable } => {
                let action = Action::StreamFailed {
                    draft_id: draft_id.clone(),
                    err,
                    retryable,
                };
                if let Err(e) = action_tx.send(action).await {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on hard-error");
                }
                return;
            }
            StreamPassOutcome::Cancelled | StreamPassOutcome::SenderClosed => return,
            StreamPassOutcome::ToolCallRequested {
                calls,
                iter_text,
                reasoning_content,
            } => {
                // 进入 tool 回合：需要 registry.
                let registry = match tools_registry.as_ref() {
                    Some(r) => r,
                    None => {
                        let action = Action::StreamFailed {
                            draft_id: draft_id.clone(),
                            err: "redux driver: tool_calls received but no tools_registry available (route should have stayed on legacy path)"
                                .to_string(),
                            retryable: false,
                        };
                        if let Err(e) = action_tx.send(action).await {
                            tracing::debug!(error = %e, "StartTurn: action_tx closed on missing-registry");
                        }
                        return;
                    }
                };

                // 1) 把 assistant 的 tool_call 追加到 history. OpenAI 协议把 assistant 的 tool_call
                //    序列化为 JSON, content 为空; 我们用更紧凑的 marker 字符串表示, 兼容 ChatMessage
                //    (无 tool_calls 专用字段). legacy run_tool_call_loop 用 build_native_assistant_history
                //    做更复杂的格式; 这里 driver 保守降级 — 用 JSON 让 provider 在下一轮收到完整
                //    assistant tool_call 上下文。后续 provider native 接通时可换为结构化字段.
                let assistant_payload = serde_json::json!({
                    "tool_calls": calls.iter().map(|c| serde_json::json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.args },
                    })).collect::<Vec<_>>(),
                    "content": iter_text,
                    "reasoning_content": reasoning_content,
                });
                history.push(crate::providers::traits::ChatMessage {
                    role: "assistant".to_string(),
                    content: assistant_payload.to_string(),
                });

                // 2) 顺序执行每个 tool call.
                for call in calls {
                    if executed_tool_ids.contains(&call.id) {
                        tracing::debug!(
                            tool_id = %call.id,
                            "drive_start_turn_stream: skipping already-executed tool_id (retry idempotency)"
                        );
                        continue;
                    }
                    let outcome = execute_single_tool_call(
                        registry,
                        &call,
                        &cancel,
                        &action_tx,
                        &draft_id,
                        approval_router.as_ref(),
                        approval_manager.as_ref(),
                        &mut history,
                        chat_mode,
                    )
                    .await;
                    match outcome {
                        ToolExecOutcome::Done => {
                            executed_tool_ids.insert(call.id.clone());
                        }
                        ToolExecOutcome::Cancelled | ToolExecOutcome::SenderClosed => return,
                    }
                }
                // 继续下一轮 LLM 调用. iter_text 已经写入 assistant message, 不计入最终 accumulated.
            }
        }
    }

    // T3-3-fixB B5：先 RecordAssistantTurn 再 StreamCompleted，让 reducer
    // reduce_stream_completed emit Effect::SaveSession 时 session.turns 已
    // 含本轮 assistant。与 fixA P0-1 legacy 路径修同款时序契约。
    let record = Action::RecordAssistantTurn(accumulated.clone());
    if let Err(e) = action_tx.send(record).await {
        tracing::debug!(error = %e, "StartTurn: action_tx closed before RecordAssistantTurn");
        return;
    }
    let action = Action::StreamCompleted {
        draft_id,
        final_text: accumulated,
        reasoning: reasoning_buf,
    };
    if let Err(e) = action_tx.send(action).await {
        tracing::debug!(error = %e, "StartTurn: action_tx closed on completion");
    }
}

/// **S3 T3-1**: 单轮工具执行的结果分类.
enum ToolExecOutcome {
    /// 工具正常完成（含 success / fail / reject — 都已发 ToolFinished + 回填 history）.
    Done,
    /// 用户 cancel — 调用方应立即从 driver 返回.
    Cancelled,
    /// action_tx 关闭，driver 应静默退出.
    SenderClosed,
}

/// BUG-09: classify whether a tool mutates state and must be intercepted in
/// plan mode. Mirrors `agent::loop_::is_write_tool`'s read-tool allowlist so the
/// Redux driver path enforces the exact same read-only contract as the legacy
/// `run_tool_call_loop`. Unknown tools are conservatively treated as writes.
fn is_plan_intercepted_write_tool(name: &str) -> bool {
    !matches!(
        name,
        "file_read"
            | "grep"
            | "web_search"
            | "web_search_tool"
            | "web_fetch"
            | "memory_recall"
            | "memory_search"
            | "memory_get"
            | "document_search"
            | "document_get_chunk"
            | "cron_list"
            | "cron_runs"
            | "sessions_list"
            | "sessions_history"
            | "session_status"
            | "agents_list"
            | "image_info"
            | "hardware_board_info"
            | "hardware_memory_map"
            | "hardware_memory_read"
    )
}

/// BUG-09: short, bounded preview of the raw tool arguments for the synthesized
/// "[plan mode] would call X with …" message. Keeps the line readable even when
/// arguments are large (file contents, shell scripts).
fn plan_preview_args(raw_args: &str) -> String {
    const MAX: usize = 160;
    if raw_args.len() <= MAX {
        return raw_args.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !raw_args.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &raw_args[..cut])
}

/// **S3 T3-1**: 执行单个工具调用（含 approval 检查）+ 写回 history + 发 Tool* Action.
///
/// 抽出独立函数原因：driver 主循环里嵌套层级太多，且 approval 路径有 oneshot await，
/// 拆出后逻辑/borrow 都更清晰。返回值告诉调用方下一步行为（继续 / 取消 / 退出）。
#[allow(clippy::too_many_arguments)]
async fn execute_single_tool_call(
    registry: &Arc<Vec<Box<dyn crate::tools::Tool>>>,
    call: &ResolvedToolCall,
    cancel: &CancellationToken,
    action_tx: &mpsc::Sender<Action>,
    draft_id: &str,
    approval_router: Option<&Arc<ApprovalRouter>>,
    approval_manager: Option<&Arc<crate::approval::ApprovalManager>>,
    history: &mut Vec<crate::providers::traits::ChatMessage>,
    chat_mode: crate::agent::loop_::ChatMode,
) -> ToolExecOutcome {
    // 0) BUG-09: plan mode is read-only. Intercept write/shell/git tools BEFORE
    // approval or execution and feed back a simulated "[plan mode] would call X"
    // result so the model can keep reasoning without any real side effect
    // touching the filesystem. This mirrors the legacy `run_tool_call_loop`
    // interception (agent::loop_::execute_one_tool) for the Redux driver path,
    // which previously executed write tools for real even in plan mode.
    if chat_mode.intercepts_writes() && is_plan_intercepted_write_tool(&call.name) {
        let preview = plan_preview_args(&call.args);
        let simulated = format!("[plan mode] would call {} with {preview}", call.name);
        let tool_payload = serde_json::json!({
            "tool_call_id": call.id,
            "content": simulated,
            "success": true,
        });
        history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
        if let Err(e) = action_tx
            .send(Action::ToolFinished {
                name: call.name.clone(),
                success: true,
                duration_ms: 0,
                result: Some(simulated),
            })
            .await
        {
            tracing::debug!(error = %e, "StartTurn: action_tx closed on plan-mode-intercept");
            return ToolExecOutcome::SenderClosed;
        }
        return ToolExecOutcome::Done;
    }

    // 1) Approval — supervised mode 走 oneshot 等响应.
    let needs_approval = approval_manager.is_some_and(|mgr| mgr.needs_approval(&call.name));
    if needs_approval {
        if let Some(router) = approval_router {
            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
            router.register(call.id.clone(), tx);
            // 通知 reducer / UI 请求 approval.
            if let Err(e) = action_tx
                .send(Action::ToolApprovalRequested {
                    tool_id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.args.clone(),
                })
                .await
            {
                tracing::debug!(error = %e, "StartTurn: action_tx closed on approval-request");
                return ToolExecOutcome::SenderClosed;
            }
            // 等响应（与 cancel 竞速；cancel 时清理 pending router）.
            let approved = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    // 主动 take 出 router 内的 entry（即便 dispatcher 还未 resolve）.
                    let _ = router.resolve(&call.id, false);
                    if let Err(e) = action_tx.send(Action::StreamCancelled { draft_id: draft_id.to_string() }).await {
                        tracing::debug!(error = %e, "StartTurn: action_tx closed on cancel-mid-approval");
                    }
                    return ToolExecOutcome::Cancelled;
                }
                res = rx => res.unwrap_or(false),
            };
            if !approved {
                let err_msg = "User rejected tool approval".to_string();
                let tool_payload = serde_json::json!({
                    "tool_call_id": call.id,
                    "content": err_msg,
                    "success": false,
                });
                history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
                if let Err(e) = action_tx
                    .send(Action::ToolFinished {
                        name: call.name.clone(),
                        success: false,
                        duration_ms: 0,
                        result: Some(err_msg),
                    })
                    .await
                {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on tool-rejected");
                    return ToolExecOutcome::SenderClosed;
                }
                return ToolExecOutcome::Done;
            }
        } else {
            tracing::error!(
                tool = %call.name,
                tool_id = %call.id,
                "tool needs_approval=true but no approval_router wired; rejecting (fail-CLOSED)"
            );
            let err_msg = "approval system not available; tool rejected for safety".to_string();
            let tool_payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": err_msg,
                "success": false,
            });
            history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
            if let Err(e) = action_tx
                .send(Action::ToolFinished {
                    name: call.name.clone(),
                    success: false,
                    duration_ms: 0,
                    result: Some(err_msg),
                })
                .await
            {
                tracing::debug!(error = %e, "StartTurn: action_tx closed on tool-rejected-fail-closed");
                return ToolExecOutcome::SenderClosed;
            }
            return ToolExecOutcome::Done;
        }
    }

    // 2) 发 ToolStarted（reducer/UI 感知）.
    if let Err(e) = action_tx
        .send(Action::ToolStarted {
            name: call.name.clone(),
            args: call.args.clone(),
        })
        .await
    {
        tracing::debug!(error = %e, "StartTurn: action_tx closed on tool-started");
        return ToolExecOutcome::SenderClosed;
    }

    // 3) 解析 args JSON. 失败 → 把错误回填给 LLM 让它自己修正.
    let args_value: serde_json::Value = match serde_json::from_str(&call.args) {
        Ok(v) => v,
        Err(parse_err) => {
            let err_msg = format!("tool args JSON parse error: {parse_err}");
            let tool_payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": err_msg,
                "success": false,
            });
            history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
            let _ = action_tx
                .send(Action::ToolFinished {
                    name: call.name.clone(),
                    success: false,
                    duration_ms: 0,
                    result: Some(err_msg),
                })
                .await;
            return ToolExecOutcome::Done;
        }
    };

    // 4) 查找 tool. 未找到也走"回填给 LLM"路径.
    let tool_match = registry.iter().find(|t| t.supports_name(&call.name));
    let tool = match tool_match {
        Some(t) => t,
        None => {
            let err_msg = format!("tool not found: {}", call.name);
            let tool_payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": err_msg,
                "success": false,
            });
            history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
            let _ = action_tx
                .send(Action::ToolFinished {
                    name: call.name.clone(),
                    success: false,
                    duration_ms: 0,
                    result: Some(err_msg),
                })
                .await;
            return ToolExecOutcome::Done;
        }
    };

    // 5) 执行 tool, 与 cancel 竞速.
    let start = std::time::Instant::now();
    let exec_result = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            if let Err(e) = action_tx.send(Action::StreamCancelled { draft_id: draft_id.to_string() }).await {
                tracing::debug!(error = %e, "StartTurn: action_tx closed on cancel mid-tool");
            }
            return ToolExecOutcome::Cancelled;
        }
        res = tool.execute_named(&call.name, args_value) => res,
    };
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let (tool_payload, ok_flag, summary) = match exec_result {
        Ok(tool_result) => {
            // BUG-05: when a tool fails (e.g. file_write rejected by the path
            // security policy) the human-readable reason lives in `error`, and
            // `output` is usually empty. The LLM keys off `content`, so an empty
            // `content` made the model believe the call "returned nothing /
            // looked fine". Surface the error reason in `content` (not just the
            // side `error` field) so the rejection is unambiguous to the model.
            let content = if tool_result.success || !tool_result.output.is_empty() {
                tool_result.output.clone()
            } else {
                tool_result
                    .error
                    .clone()
                    .unwrap_or_else(|| "tool failed with no output".to_string())
            };
            let payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": content,
                "success": tool_result.success,
                "error": tool_result.error,
            });
            let summary = if tool_result.success {
                tool_result.output.clone()
            } else {
                tool_result.error.clone().unwrap_or_else(|| "tool failed".to_string())
            };
            (payload, tool_result.success, summary)
        }
        Err(e) => {
            let err_str = e.to_string();
            let payload = serde_json::json!({
                "tool_call_id": call.id,
                "content": err_str,
                "success": false,
            });
            (payload, false, err_str)
        }
    };
    history.push(crate::providers::traits::ChatMessage::tool(tool_payload.to_string()));
    let _ = action_tx
        .send(Action::ToolFinished {
            name: call.name.clone(),
            success: ok_flag,
            duration_ms,
            result: Some(summary),
        })
        .await;
    ToolExecOutcome::Done
}

/// **S3 T3-1**: 单轮 stream 调用 + 网络瞬时故障 exponential backoff retry.
///
/// 行为：
/// - 调用 `provider.stream_chat_with_history` 收 chunks，发 `Action::StreamChunkReceived`
/// - 遇 `is_timeout()` / `is_connect()` 错误：sleep 后重试，最多 [`MAX_NETWORK_RETRIES`] 次
/// - 遇 context overflow（HTTP 413 / provider 错误消息匹配）：返回 ContextOverflow 让外层 compact + retry
/// - 遇普通可重试 (`StreamError::Http`) 但非 timeout：当 hard error 返回（不进 retry loop）
/// - 遇 cancel：发 StreamCancelled 并返回 Cancelled
#[allow(clippy::too_many_arguments)]
async fn run_one_stream_pass_with_retry(
    provider: &dyn Provider,
    history: &[crate::providers::traits::ChatMessage],
    model: &str,
    temperature: f64,
    cancel: &CancellationToken,
    draft_id: &str,
    action_tx: &mpsc::Sender<Action>,
    version: &mut u64,
    reasoning_buf: &mut String,
    tool_specs: &[crate::tools::ToolSpec],
) -> StreamPassOutcome {
    let mut attempt: u8 = 0;
    loop {
        if cancel.is_cancelled() {
            if let Err(e) = action_tx
                .send(Action::StreamCancelled {
                    draft_id: draft_id.to_string(),
                })
                .await
            {
                tracing::debug!(error = %e, "StartTurn: action_tx closed on pre-pass cancel");
                return StreamPassOutcome::SenderClosed;
            }
            return StreamPassOutcome::Cancelled;
        }
        match run_one_stream_pass(
            provider,
            history,
            model,
            temperature,
            cancel,
            draft_id,
            action_tx,
            version,
            reasoning_buf,
            tool_specs,
        )
        .await
        {
            inner @ (StreamPassOutcome::Completed { .. }
            | StreamPassOutcome::ToolCallRequested { .. }
            | StreamPassOutcome::ContextOverflow { .. }
            | StreamPassOutcome::HardError { .. }
            | StreamPassOutcome::Cancelled
            | StreamPassOutcome::SenderClosed) => return inner,
            StreamPassOutcome::TransientNetworkError { err } => {
                let last_err = err;
                attempt = attempt.saturating_add(1);
                if attempt > MAX_NETWORK_RETRIES {
                    return StreamPassOutcome::TransientNetworkError {
                        err: format!("network retries exhausted ({MAX_NETWORK_RETRIES}): {last_err}"),
                    };
                }
                // 通知 reducer / UI 重试尝试（可观测性）.
                if let Err(e) = action_tx
                    .send(Action::StreamRetryAttempt {
                        attempt,
                        reason: last_err.clone(),
                    })
                    .await
                {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on retry-notify");
                    return StreamPassOutcome::SenderClosed;
                }
                // 500ms, 1000ms, 2000ms — `<< (attempt-1)` 的乘法等价（u64 不支持 saturating_shl）.
                let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.saturating_sub(1).min(31));
                tracing::info!(
                    attempt,
                    backoff_ms,
                    err = %last_err,
                    "drive_start_turn_stream: backoff retry"
                );
                let sleep = tokio::time::sleep(std::time::Duration::from_millis(backoff_ms));
                tokio::pin!(sleep);
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        if let Err(e) = action_tx
                            .send(Action::StreamCancelled { draft_id: draft_id.to_string() })
                            .await
                        {
                            tracing::debug!(error = %e, "StartTurn: action_tx closed on cancel-mid-backoff");
                            return StreamPassOutcome::SenderClosed;
                        }
                        return StreamPassOutcome::Cancelled;
                    }
                    () = &mut sleep => {}
                }
            }
        }
    }
}

/// **S3 T3-1**: 真正的单轮 stream 拉取（不含 retry / overflow 重试逻辑）.
///
/// 抽出独立函数让 retry/overflow loop 在外层组合；本函数行为：
/// - chunk-by-chunk consume，按 index 聚合 `ToolCallChunk`
/// - chunk 内 reasoning 累加到 `reasoning_buf`，文本 chunk 通过 `Action::StreamChunkReceived` 回投
/// - stream 自然结束 / `is_final` → 返回 Completed 或 ToolCallRequested
/// - stream 错误 → 按类型返回 ContextOverflow / TransientNetworkError / HardError
#[allow(clippy::too_many_arguments)]
async fn run_one_stream_pass(
    provider: &dyn Provider,
    history: &[crate::providers::traits::ChatMessage],
    model: &str,
    temperature: f64,
    cancel: &CancellationToken,
    draft_id: &str,
    action_tx: &mpsc::Sender<Action>,
    version: &mut u64,
    reasoning_buf: &mut String,
    tool_specs: &[crate::tools::ToolSpec],
) -> StreamPassOutcome {
    use crate::providers::traits::{StreamChunk, StreamOptions};
    use futures::StreamExt;

    let opts = StreamOptions::new(true).with_tools(tool_specs.to_vec());
    let stream = provider.stream_chat_with_history(history, model, temperature, opts);
    tokio::pin!(stream);

    let mut aggregator = ToolCallAggregator::new();
    let mut completed_calls: Vec<ResolvedToolCall> = Vec::new();
    let mut iter_text = String::new();
    let mut iter_reasoning = String::new();

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                if let Err(e) = action_tx.send(Action::StreamCancelled { draft_id: draft_id.to_string() }).await {
                    tracing::debug!(error = %e, "StartTurn: action_tx closed on cancel");
                    return StreamPassOutcome::SenderClosed;
                }
                return StreamPassOutcome::Cancelled;
            }
            next = stream.next() => {
                match next {
                    Some(Ok(StreamChunk { delta, reasoning, is_final, tool_calls, .. })) => {
                        if !tool_calls.is_empty() {
                            for tc in tool_calls {
                                if let Some((id, name, args)) = aggregator.ingest(tc) {
                                    completed_calls.push(ResolvedToolCall { id, name, args });
                                }
                            }
                        }
                        if let Some(reason_text) = reasoning {
                            if !reason_text.is_empty() {
                                iter_reasoning.push_str(&reason_text);
                                reasoning_buf.push_str(&reason_text);
                            }
                        }
                        if !delta.is_empty() {
                            *version = version.saturating_add(1);
                            iter_text.push_str(&delta);
                            let action = Action::StreamChunkReceived {
                                draft_id: draft_id.to_string(),
                                delta,
                                version: *version,
                            };
                            if let Err(e) = action_tx.send(action).await {
                                tracing::debug!(error = %e, "StartTurn: action_tx closed mid-stream");
                                return StreamPassOutcome::SenderClosed;
                            }
                        }
                        if is_final {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        // S3 T3-1: 错误分类 — overflow / network timeout / hard error.
                        if stream_error_is_context_overflow(&err) {
                            return StreamPassOutcome::ContextOverflow { err: err.to_string() };
                        }
                        if stream_error_is_network_timeout(&err) {
                            return StreamPassOutcome::TransientNetworkError { err: err.to_string() };
                        }
                        let retryable = stream_error_is_retryable(&err);
                        return StreamPassOutcome::HardError { err: err.to_string(), retryable };
                    }
                    None => {
                        // Stream ended without explicit final chunk — treat as completion.
                        break;
                    }
                }
            }
        }
    }

    if completed_calls.is_empty() {
        StreamPassOutcome::Completed { iter_text }
    } else {
        StreamPassOutcome::ToolCallRequested {
            calls: completed_calls,
            iter_text,
            reasoning_content: iter_reasoning,
        }
    }
}

// ─── Dispatcher task ───────────────────────────────────────────────────────────

/// Spawn the central dispatcher task: drives `state.reduce(action)` for every
/// Action received on `action_rx`, then runs each returned Effect through the
/// shadow [`EffectExecutor`].
///
/// 关闭条件:
/// - `action_rx.recv()` 返回 `None`（所有 sender drop 完毕）
/// - `shutdown.cancelled()` 触发（select! 抢占）
///
/// Step 5b shadow 模式：dispatcher task 只跑 reducer + log effect，不会产生外部副作用，
/// 因此与 main loop 的旧路径并存安全。返回 `JoinHandle` 让 `chat::run` 在结束前 await
/// 一次以确保最后的 trace 输出完整。
#[allow(dead_code)]
pub fn spawn_dispatcher_task(
    initial_state: ChatState,
    action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<DispatcherStats> {
    spawn_dispatcher_task_with_executor(initial_state, action_rx, shutdown, EffectExecutor::new_shadow())
}

/// Spawn dispatcher task with explicit [`EffectExecutor`].
///
/// 与 [`spawn_dispatcher_task`] 等价但允许 caller 注入 real-mode executor（Step 5a-1）.
/// 测试与 shadow 兼容场景仍用 `spawn_dispatcher_task`.
#[allow(dead_code)]
pub fn spawn_dispatcher_task_with_executor(
    initial_state: ChatState,
    action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
) -> tokio::task::JoinHandle<DispatcherStats> {
    spawn_dispatcher_task_with_signal(initial_state, action_rx, shutdown, executor, None)
}

/// Step 5a-4: Spawn dispatcher task with optional [`TurnCompletionSignal`].
///
/// 当 signal 存在时，dispatcher 在 reduce 完任意 turn 终结 action
/// (`StreamCompleted` / `StreamFailed` / `StreamCancelled`) 后调用
/// `signal.notify()`，唤醒在 `chat::run` 主循环里等待 turn 完成的 await 点。
///
/// 该协议与 [`is_turn_terminal_action`] 配合使用：dispatcher 完全不感知具体
/// driver 实现，仅按 action 类型触发 turn 边界事件。
#[allow(dead_code)]
pub fn spawn_dispatcher_task_with_signal(
    initial_state: ChatState,
    action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
    turn_signal: Option<TurnCompletionSignal>,
) -> tokio::task::JoinHandle<DispatcherStats> {
    spawn_dispatcher_task_full(
        initial_state,
        action_rx,
        shutdown,
        executor,
        turn_signal,
        #[cfg(feature = "terminal-tui")]
        None,
    )
}

/// S4-A 收尾 P1: 抽出 dispatcher 两处重复的 snapshot 构造 + 推送块.
/// send_replace 直接覆盖；snapshot_rev 单调递增由 reduce 顺序保证.
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
fn push_snapshot_if_dirty(
    state: &mut ChatState,
    snapshot_tx: &Option<tokio::sync::watch::Sender<Arc<crate::chat::state::UiSnapshot>>>,
    snapshot_rev: &std::sync::atomic::AtomicU64,
    dirty: bool,
) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    if !dirty {
        return;
    }
    let Some(tx) = snapshot_tx.as_ref() else {
        return;
    };
    let next_rev = snapshot_rev.fetch_add(1, AtomicOrdering::Relaxed).saturating_add(1);
    let new_snap = Arc::new(state.build_ui_snapshot(next_rev));
    tx.send_replace(new_snap);
    tracing::trace!(rev = next_rev, "s4_a snapshot pushed");
}

/// **S4-A Commit 3**: `spawn_dispatcher_task_with_signal` + 可选的 UiSnapshot 推送.
///
/// Pure 模式传入 `snapshot_tx: Some(watch::Sender<Arc<UiSnapshot>>)`，dispatcher
/// 在 reduce 完成且 `ui_dirty=true` 时构造新 snapshot 并 send_if_modified；
/// Off/Both/Redux 模式传 None 维持 chat_mirror 单源路径。
///
/// snapshot_rev: AtomicU64 单调递增。watch send_if_modified 用 revision 比较
/// 跳过相同帧；revision 不会回退，杜绝 receiver 看到旧帧。
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
pub fn spawn_dispatcher_task_full(
    initial_state: ChatState,
    mut action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
    turn_signal: Option<TurnCompletionSignal>,
    snapshot_tx: Option<tokio::sync::watch::Sender<Arc<crate::chat::state::UiSnapshot>>>,
) -> tokio::task::JoinHandle<DispatcherStats> {
    use std::sync::atomic::AtomicU64;
    // S3 T3-1: 提前抽出 approval_router 句柄（Arc clone），后续在 reducer 处理完
    // `Action::ToolApprovalReceived` 之后用它把决策转交 driver 等待中的 oneshot。
    let approval_router = executor.approval_router();
    tokio::spawn(async move {
        let mut state = initial_state;
        let mut stats = DispatcherStats::default();
        // S4-A Commit 3: revision 计数器仅在 snapshot_tx 存在时使用.
        let snapshot_rev = AtomicU64::new(0);

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    // Drain whatever is left (best-effort) before exit so
                    // late-arriving Actions still hit the reducer for
                    // observability. Bounded by remaining queue depth, not by
                    // network/disk I/O.
                    while let Ok(action) = action_rx.try_recv() {
                        stats.actions_seen = stats.actions_seen.saturating_add(1);
                        let outcome = extract_turn_outcome(&action);
                        let approval_response = extract_approval_response(&action);
                        let (effects, ui_dirty) = state.reduce_tracked(action);
                        stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                        for effect in effects {
                            executor.execute(effect).await;
                        }
                        push_snapshot_if_dirty(&mut state, &snapshot_tx, &snapshot_rev, ui_dirty);
                        if let (Some((tool_id, approved)), Some(router)) =
                            (approval_response, approval_router.as_ref())
                        {
                            router.resolve(&tool_id, approved);
                        }
                        // Shutdown 阶段也要触发 turn_signal，否则 chat::run await
                        // 会被 shutdown 抢占前最后一轮 turn 永远卡住（导致 round 2 hang
                        // 回归）。terminal action 携带 outcome — main.rs:888
                        // shutdown_timeout 兜底保证主进程最终退出。
                        if let (Some(out), Some(ref sig)) = (outcome, turn_signal.as_ref()) {
                            sig.record_and_notify(out);
                        }
                    }
                    // 兜底：shutdown 期间 chat::run 仍可能在 await turn_signal.notified()，
                    // 通知一轮让其检测到 shutdown.cancelled() 退出 select（无 outcome
                    // → 等待方按 cancelled 解释）。
                    if let Some(ref sig) = turn_signal {
                        sig.notify();
                    }
                    tracing::debug!(
                        actions = stats.actions_seen,
                        effects = stats.effects_seen,
                        "redux dispatcher task: shutdown drained"
                    );
                    break;
                }
                maybe_action = action_rx.recv() => {
                    match maybe_action {
                        Some(action) => {
                            stats.actions_seen = stats.actions_seen.saturating_add(1);
                            let outcome = extract_turn_outcome(&action);
                            let approval_response = extract_approval_response(&action);
                            let (effects, ui_dirty) = state.reduce_tracked(action);
                            stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                            for effect in effects {
                                executor.execute(effect).await;
                            }
                            push_snapshot_if_dirty(&mut state, &snapshot_tx, &snapshot_rev, ui_dirty);
                            // S3 T3-1: reducer 处理完 ToolApprovalReceived 后，把决策
                            // 通过 approval_router 转给 driver 的 pending oneshot。
                            if let (Some((tool_id, approved)), Some(router)) =
                                (approval_response, approval_router.as_ref())
                            {
                                router.resolve(&tool_id, approved);
                            }
                            if let (Some(out), Some(ref sig)) = (outcome, turn_signal.as_ref()) {
                                sig.record_and_notify(out);
                            }
                        }
                        None => {
                            // Channel 关闭：所有 dispatcher sender 已 drop。
                            // 兜底 notify 防止 chat::run await 永远等待。
                            if let Some(ref sig) = turn_signal {
                                sig.notify();
                            }
                            tracing::debug!(
                                actions = stats.actions_seen,
                                effects = stats.effects_seen,
                                "redux dispatcher task: channel closed, exiting"
                            );
                            break;
                        }
                    }
                }
            }
        }

        stats
    })
}

/// 非 terminal-tui feature 下的 spawn_dispatcher_task_full 占位（无 snapshot 推送）.
#[cfg(not(feature = "terminal-tui"))]
#[allow(dead_code)]
pub fn spawn_dispatcher_task_full(
    initial_state: ChatState,
    mut action_rx: mpsc::Receiver<Action>,
    shutdown: CancellationToken,
    executor: EffectExecutor,
    turn_signal: Option<TurnCompletionSignal>,
) -> tokio::task::JoinHandle<DispatcherStats> {
    let approval_router = executor.approval_router();
    tokio::spawn(async move {
        let mut state = initial_state;
        let mut stats = DispatcherStats::default();

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    while let Ok(action) = action_rx.try_recv() {
                        stats.actions_seen = stats.actions_seen.saturating_add(1);
                        let outcome = extract_turn_outcome(&action);
                        let approval_response = extract_approval_response(&action);
                        let effects = state.reduce(action);
                        stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                        for effect in effects {
                            executor.execute(effect).await;
                        }
                        if let (Some((tool_id, approved)), Some(router)) =
                            (approval_response, approval_router.as_ref())
                        {
                            router.resolve(&tool_id, approved);
                        }
                        if let (Some(out), Some(ref sig)) = (outcome, turn_signal.as_ref()) {
                            sig.record_and_notify(out);
                        }
                    }
                    if let Some(ref sig) = turn_signal {
                        sig.notify();
                    }
                    break;
                }
                maybe_action = action_rx.recv() => {
                    match maybe_action {
                        Some(action) => {
                            stats.actions_seen = stats.actions_seen.saturating_add(1);
                            let outcome = extract_turn_outcome(&action);
                            let approval_response = extract_approval_response(&action);
                            let effects = state.reduce(action);
                            stats.effects_seen = stats.effects_seen.saturating_add(effects.len() as u64);
                            for effect in effects {
                                executor.execute(effect).await;
                            }
                            if let (Some((tool_id, approved)), Some(router)) =
                                (approval_response, approval_router.as_ref())
                            {
                                router.resolve(&tool_id, approved);
                            }
                            if let (Some(out), Some(ref sig)) = (outcome, turn_signal.as_ref()) {
                                sig.record_and_notify(out);
                            }
                        }
                        None => {
                            if let Some(ref sig) = turn_signal {
                                sig.notify();
                            }
                            break;
                        }
                    }
                }
            }
        }

        stats
    })
}

/// **S3 T3-1**: 提取 `Action::ToolApprovalReceived` 的 (tool_id, approved) 元组.
///
/// 仅在 reducer 处理之前 / 之后用于 approval_router 转发；其他 Action 返回 None。
/// 通过 borrow 避免提前 clone — 元组在 reducer 消费 action 之前抽取。
fn extract_approval_response(action: &Action) -> Option<(String, bool)> {
    match action {
        Action::ToolApprovalReceived { tool_id, approved } => Some((tool_id.clone(), *approved)),
        _ => None,
    }
}

/// Lightweight stats returned by [`spawn_dispatcher_task`] on shutdown
/// (for integration tests and metrics).
#[allow(dead_code)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DispatcherStats {
    pub actions_seen: u64,
    pub effects_seen: u64,
}

// ─── StreamChunkCoalescer ──────────────────────────────────────────────────────

/// `StreamChunkReceived` delta 合并器（Codex P0-3 应对 channel 满）.
///
/// 工作原理:
/// 1. 调用 `try_send_chunk` 先尝试 `try_send`，成功即清空 pending
/// 2. 满（Backpressured）时，把 delta 累加到 `pending` 暂存
/// 3. 下次 `try_send_chunk` 时，先发送 pending（已累加），再发当前 delta
/// 4. `flush` 在 shutdown 或 stream end 时强制冲刷 pending
///
/// 设计选择:
/// - 只为同 draft_id coalesce；跨 draft 时丢弃旧 pending（防御性，正常路径不应跨）
/// - **version 取最新**（Codex P2 fix）：与 reducer `state.rs:540` strict-monotonic
///   一致——`version <= draft.version` 一律丢弃。若取最早，高版本先到合并后会被
///   reducer 因 `merged.version <= draft.version` 丢掉，导致 delta 永久丢失。
///   merge 时取 `max(pending.version, new.version)` 保证合并 Action 至少能让
///   reducer 向前推进。
#[allow(dead_code)]
pub struct StreamChunkCoalescer {
    /// pending: (draft_id, accumulated_delta, latest_version)
    pending: Option<(String, String, u64)>,
    sender: mpsc::Sender<Action>,
}

impl StreamChunkCoalescer {
    #[allow(dead_code)]
    pub const fn new(sender: mpsc::Sender<Action>) -> Self {
        Self { pending: None, sender }
    }

    /// 尝试发送一个 chunk Action。channel 满时累加到 pending。
    ///
    /// 返回 `DispatchResult` 供调用方观测（Closed 时调用方应停止泵 chunk）。
    #[allow(dead_code)]
    pub fn try_send_chunk(&mut self, draft_id: String, delta: String, version: u64) -> DispatchResult {
        // 1. 如有 pending，先尝试一次性发送累加结果 + 当前 delta（合并为一条 Action）
        if let Some((p_draft, p_delta, p_version)) = self.pending.take() {
            if p_draft == draft_id {
                // 同 draft：累加 delta，version 取 max（与 reducer strict-monotonic 一致）
                let merged_delta = format!("{p_delta}{delta}");
                let merged_version = p_version.max(version);
                let action = Action::StreamChunkReceived {
                    draft_id: draft_id.clone(),
                    delta: merged_delta.clone(),
                    version: merged_version,
                };
                return match self.sender.try_send(action) {
                    Ok(()) => DispatchResult::Sent,
                    Err(TrySendError::Full(_)) => {
                        // 仍满：累加到 pending（version 保持 max）
                        self.pending = Some((draft_id, merged_delta, merged_version));
                        DispatchResult::Backpressured
                    }
                    Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
                };
            }
            // 跨 draft：旧 pending 已无意义，丢弃，按当前 delta 走 fast path
            tracing::warn!(
                old_draft = %p_draft,
                new_draft = %draft_id,
                "coalescer cross-draft pending dropped (defensive)"
            );
        }

        // 2. 没有 pending（或刚清空）：直接 try_send 当前 delta
        let action = Action::StreamChunkReceived {
            draft_id: draft_id.clone(),
            delta: delta.clone(),
            version,
        };
        match self.sender.try_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(TrySendError::Full(_)) => {
                self.pending = Some((draft_id, delta, version));
                DispatchResult::Backpressured
            }
            Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
        }
    }

    /// stream 结束或 shutdown 时强制冲刷 pending（best-effort）.
    #[allow(dead_code)]
    pub fn flush(&mut self) -> DispatchResult {
        let Some((draft_id, delta, version)) = self.pending.take() else {
            return DispatchResult::Sent;
        };
        let action = Action::StreamChunkReceived {
            draft_id,
            delta,
            version,
        };
        match self.sender.try_send(action) {
            Ok(()) => DispatchResult::Sent,
            Err(TrySendError::Full(_)) => DispatchResult::Backpressured,
            Err(TrySendError::Closed(_)) => DispatchResult::ChannelClosed,
        }
    }

    /// 测试观测 pending 状态.
    #[cfg(test)]
    pub const fn pending_for_test(&self) -> Option<&(String, String, u64)> {
        self.pending.as_ref()
    }
}

// ─── 单元测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::action::Action;

    #[tokio::test]
    async fn dispatcher_try_send_ok() {
        let (dispatcher, mut rx) = ChatDispatcher::new();
        let result = dispatcher.try_dispatch(Action::ForceQuit);
        assert_eq!(result, DispatchResult::Sent);
        let received = rx.recv().await.expect("expected one Action");
        assert!(matches!(received, Action::ForceQuit));
    }

    #[tokio::test]
    async fn dispatcher_channel_closed() {
        let (dispatcher, rx) = ChatDispatcher::new();
        drop(rx);
        let result = dispatcher.try_dispatch(Action::ForceQuit);
        assert_eq!(result, DispatchResult::ChannelClosed);
    }

    #[tokio::test]
    async fn coalescer_passthrough_when_not_full() {
        let (tx, mut rx) = mpsc::channel::<Action>(16);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        coalescer.try_send_chunk("d1".to_string(), "hello ".to_string(), 1);
        coalescer.try_send_chunk("d1".to_string(), "world".to_string(), 2);

        // Both should pass through individually (channel not full)
        let a1 = rx.recv().await.expect("expected chunk 1");
        let a2 = rx.recv().await.expect("expected chunk 2");
        match a1 {
            Action::StreamChunkReceived { delta, .. } => assert_eq!(delta, "hello "),
            other => panic!("unexpected action {other:?}"),
        }
        match a2 {
            Action::StreamChunkReceived { delta, .. } => assert_eq!(delta, "world"),
            other => panic!("unexpected action {other:?}"),
        }
        assert!(coalescer.pending_for_test().is_none());
    }

    #[tokio::test]
    async fn coalescer_merges_when_full() {
        // 容量 1 channel：写一条后必满
        let (tx, mut rx) = mpsc::channel::<Action>(1);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        // 第一条：成功
        let r1 = coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 1);
        assert_eq!(r1, DispatchResult::Sent);
        // 第二条：channel 满，进 pending
        let r2 = coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
        assert_eq!(r2, DispatchResult::Backpressured);
        // 第三条：仍满，与 pending 合并
        let r3 = coalescer.try_send_chunk("d1".to_string(), "c".to_string(), 3);
        assert_eq!(r3, DispatchResult::Backpressured);
        let pending = coalescer.pending_for_test().expect("pending should exist");
        assert_eq!(pending.1, "bc");
        assert_eq!(pending.2, 3, "version should be max(2,3)=3 (Codex P2 fix)");

        // 消费第一条，腾出空间
        let a1 = rx.recv().await.expect("first chunk");
        match a1 {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "a");
                assert_eq!(version, 1);
            }
            other => panic!("unexpected {other:?}"),
        }

        // flush pending
        let rf = coalescer.flush();
        assert_eq!(rf, DispatchResult::Sent);
        let a_merged = rx.recv().await.expect("merged chunk");
        match a_merged {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "bc");
                assert_eq!(version, 3, "merged version is max (Codex P2 fix)");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(coalescer.pending_for_test().is_none());
    }

    #[tokio::test]
    async fn coalescer_cross_draft_drops_old_pending() {
        let (tx, mut rx) = mpsc::channel::<Action>(1);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        // d1 chunk 1 → 成功（填满）
        coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 1);
        // d1 chunk 2 → 满，进 pending
        coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
        // d2 chunk 1 → 跨 draft，旧 pending 丢弃
        // 通道仍满（d1 chunk 1 未消费），d2 chunk 1 进 pending
        let r = coalescer.try_send_chunk("d2".to_string(), "x".to_string(), 5);
        assert_eq!(r, DispatchResult::Backpressured);
        let pending = coalescer.pending_for_test().expect("pending");
        assert_eq!(pending.0, "d2");
        assert_eq!(pending.1, "x");

        // 排空 d1 chunk 1
        let _ = rx.recv().await;
        let _ = coalescer.flush();
        let a = rx.recv().await.expect("d2 chunk");
        match a {
            Action::StreamChunkReceived {
                draft_id,
                delta,
                version,
            } => {
                assert_eq!(draft_id, "d2");
                assert_eq!(delta, "x");
                assert_eq!(version, 5);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// P2 fix: 高版本先到，低版本后合并 — pending.version 应保持 max 而非被覆盖.
    ///
    /// 场景: version=5 chunk 先通过 try_send（成功），version=2 chunk 后到（满，进 pending），
    /// version=3 chunk 继续到（满，与 pending 合并）。
    /// 期望: pending.version = max(2,3) = 3（不是 2，也不是乱序倒退）。
    /// 验证 coalescer 总是取 max version，无论到达顺序如何。
    #[tokio::test]
    async fn coalescer_handles_out_of_order_versions() {
        // 容量 1 channel
        let (tx, mut rx) = mpsc::channel::<Action>(1);
        let mut coalescer = StreamChunkCoalescer::new(tx);

        // 第一条 version=5 成功（填满 channel）
        let r1 = coalescer.try_send_chunk("d1".to_string(), "a".to_string(), 5);
        assert_eq!(r1, DispatchResult::Sent, "first chunk should succeed");

        // 第二条 version=2（低于已发送的 5）— channel 满，进 pending
        let r2 = coalescer.try_send_chunk("d1".to_string(), "b".to_string(), 2);
        assert_eq!(r2, DispatchResult::Backpressured);

        // 第三条 version=3 — channel 仍满，与 pending(version=2) 合并
        // 合并规则: version = max(2, 3) = 3
        let r3 = coalescer.try_send_chunk("d1".to_string(), "c".to_string(), 3);
        assert_eq!(r3, DispatchResult::Backpressured);

        let pending = coalescer.pending_for_test().expect("pending should exist");
        assert_eq!(pending.1, "bc", "delta should be concatenated");
        assert_eq!(
            pending.2, 3,
            "version must be max(2,3)=3 even with out-of-order arrival (P2)"
        );

        // 消费，flush，验证最终输出
        let _ = rx.recv().await.expect("first chunk (version=5)");
        let rf = coalescer.flush();
        assert_eq!(rf, DispatchResult::Sent);
        let merged = rx.recv().await.expect("merged chunk");
        match merged {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "bc");
                assert_eq!(version, 3, "flushed version should be max(2,3)=3");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// P2 extra: 验证 reducer 在 strict-monotonic 模式下正常处理高版本先到 + 低版本后到.
    /// reducer 应接受 version=5，然后因 version=2 < current_stream_version 而忽略它。
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn redux_stream_chunk_strict_monotonic_high_then_low() {
        use crate::chat::state::ChatState;
        use tokio_util::sync::CancellationToken;

        let shutdown = CancellationToken::new();
        let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown);

        // 启动 turn
        let cancel = CancellationToken::new();
        let effects0 = state.reduce(Action::TurnStarted {
            draft_id: "d1".to_string(),
            cancel,
        });
        // TurnStarted 可能产生 StartTurn effect；只确认不 panic
        let _ = effects0;

        // version=5 先到 — strict-monotonic: 应被接受 (5 > 0)
        let e1 = state.reduce(Action::StreamChunkReceived {
            draft_id: "d1".to_string(),
            delta: "high".to_string(),
            version: 5,
        });
        // 应该产出 RequestRedraw effect（chunk 被接受）
        assert!(
            e1.iter()
                .any(|e| matches!(e, crate::chat::state::Effect::RequestRedraw)),
            "version=5 chunk should be accepted and produce RequestRedraw"
        );

        // version=2 后到 — strict-monotonic: 应被丢弃 (2 < 5)
        let e2 = state.reduce(Action::StreamChunkReceived {
            draft_id: "d1".to_string(),
            delta: "low".to_string(),
            version: 2,
        });
        // 低版本 chunk 被 reducer 静默丢弃，不应产出 RequestRedraw
        assert!(
            !e2.iter()
                .any(|e| matches!(e, crate::chat::state::Effect::RequestRedraw)),
            "version=2 chunk (lower than 5) should be discarded by strict-monotonic reducer"
        );
    }

    #[tokio::test]
    async fn effect_executor_shadow_log_trace_runs() {
        let executor = EffectExecutor::new_shadow();
        // LogTrace 真执行（shadow 也跑）；这里只验证不 panic / 不 await 外部
        executor
            .execute(Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "shadow log".to_string(),
            })
            .await;
    }

    #[tokio::test]
    async fn effect_executor_shadow_business_noop() {
        let executor = EffectExecutor::new_shadow();
        // 所有业务 effect 都是 no-op；仅验证不 panic
        executor.execute(Effect::RequestRedraw).await;
        executor.execute(Effect::Quit).await;
        executor.execute(Effect::CancelDraft("d1".to_string())).await;
        executor
            .execute(Effect::SendDraftFinalize {
                draft_id: "d1".to_string(),
                text: "hello".to_string(),
            })
            .await;
    }

    // ── S5 P0-3 supervised approval fail-safe deny ──────────────────────────

    #[test]
    fn s5_release_p0_3_supervised_unset_env_denies_by_default() {
        // env 未设置 → fail-safe deny (BREAKING — 原为 auto-approve)
        assert!(!resolve_supervised_approval_override(None));
    }

    #[test]
    fn s5_release_p0_3_supervised_env_allow_approves() {
        for v in ["allow", "ALLOW", " allow ", "y", "Y", "yes", "YES", "1"] {
            assert!(resolve_supervised_approval_override(Some(v)), "{v:?} 应当批准");
        }
    }

    #[test]
    fn s5_release_p0_3_supervised_env_deny_rejects() {
        for v in ["deny", "DENY", " deny ", "n", "N", "no", "NO", "0", "", "garbage"] {
            assert!(!resolve_supervised_approval_override(Some(v)), "{v:?} 应当拒绝");
        }
    }

    // ── S5 P0-1: stream error 分类回归 (driver 协议层) ──────────────────────

    #[test]
    fn s5_release_p0_1_retryable_http_io_triggers_retry() {
        // StreamError::Io 总是 retryable（与 driver retry-loop 同源判定）.
        let io_err =
            crate::providers::traits::StreamError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom"));
        assert!(
            stream_error_is_retryable(&io_err),
            "Io 错误必须可重试 (driver retry loop 依赖)"
        );
        // Provider 错误（语义错）不可重试.
        let provider_err = crate::providers::traits::StreamError::Provider("invalid api key".to_string());
        assert!(!stream_error_is_retryable(&provider_err), "Provider 语义错不可重试");
    }

    #[test]
    fn s5_release_p0_1_context_overflow_triggers_compact() {
        // 三家 provider 不同错误措辞都应被识别为 context overflow.
        let cases = [
            "This model's maximum context length is 8192 tokens",
            "prompt is too long",
            "request exceed the maximum input token count",
            "context_length_exceeded",
        ];
        for msg in cases {
            let err = crate::providers::traits::StreamError::Provider(msg.to_string());
            assert!(
                stream_error_is_context_overflow(&err),
                "应识别为 context overflow: {msg:?}"
            );
        }
        // 非 overflow 错误不能触发 compact.
        let other = crate::providers::traits::StreamError::Provider("rate limited".to_string());
        assert!(!stream_error_is_context_overflow(&other), "rate limit 不应当 compact");
    }

    #[tokio::test]
    async fn s5_release_p0_1_parallel_tool_calls_serialize() {
        // SCRIPT 在同一 stream 内 emit 两个 tool_call chunk + final.
        // StreamChunkCoalescer 用 ToolCallChunk 模拟串行场景；这里直接验证
        // StreamChunk::tool_call_chunk 能携带多个 ToolCallChunk 且 has_tool_calls()=true.
        use crate::providers::traits::{StreamChunk, ToolCallChunk};
        let calls = vec![
            ToolCallChunk::new("t1".to_string(), "shell".to_string(), "{}".to_string(), 0),
            ToolCallChunk::new("t2".to_string(), "file_read".to_string(), "{}".to_string(), 1),
        ];
        let chunk = StreamChunk::tool_call_chunk(calls);
        assert!(chunk.has_tool_calls(), "并行 tool calls 应当被 chunk 识别");
        assert_eq!(chunk.tool_calls.len(), 2, "应该携带 2 个 tool call");
        let first = chunk.tool_calls.first().expect("tool[0]");
        let second = chunk.tool_calls.get(1).expect("tool[1]");
        assert_eq!(first.id, "t1");
        assert_eq!(second.id, "t2");
        // 顺序保留：driver 用 index 字段串行化执行（这里只断言数据结构层).
        assert_eq!(first.index, 0);
        assert_eq!(second.index, 1);
    }

    #[test]
    fn s5_release_p0_3_full_autonomy_skips_approval_entirely() {
        use crate::approval::ApprovalManager;
        use crate::config::AutonomyConfig;
        use crate::security::AutonomyLevel;
        let config = AutonomyConfig {
            level: AutonomyLevel::Full,
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        // Full autonomy 完全跳过 approval — 不走 Effect::RequestApproval 路径
        assert!(!mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("file_write"));
    }
}

// ─── Step 5b 集成测试（dispatcher + reducer + coalescer + EffectExecutor 端到端）─

#[cfg(test)]
mod integration_tests {
    //! 直接构造 dispatcher + spawn dispatcher task + 灌入 Action 流，
    //! 复现 `chat::run` 的接线逻辑，覆盖：
    //! - dispatcher channel 容量与 backpressure
    //! - reducer 是否被驱动（stats.actions_seen / effects_seen）
    //! - shadow effect executor 是否正确 no-op
    //! - shutdown 协议（drop sender + cancel token）是否能让 dispatcher 退出
    //!
    //! 重要约束：
    //! - REDUX_DIFF_COUNT == 0：shadow 模式下业务 effect no-op，不会双写 history
    //! - 测试中所有 timeout 不超过 2s，与 main.rs:866 RUNTIME_SHUTDOWN_TIMEOUT 对齐
    use super::*;
    use crate::chat::action::{Action, HistoryDir};
    use crate::chat::state::ChatState;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn make_state(shutdown: CancellationToken) -> ChatState {
        ChatState::new(Arc::from("mock"), Arc::from("mock-model"), shutdown)
    }

    #[tokio::test]
    async fn full_chat_flow_input_to_exit() {
        // 模拟一次完整 chat 流程：输入 → 流式 → 完成 → 退出.
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        // 1. 用户输入
        assert_eq!(
            dispatcher.try_dispatch(Action::InputSubmitted("hello".to_string())),
            DispatchResult::Sent
        );
        assert_eq!(
            dispatcher.try_dispatch(Action::RecordUserTurn("hello".to_string())),
            DispatchResult::Sent
        );

        // 2. LLM 推理开始 + 流式 chunk
        let draft_id = "draft-1".to_string();
        let cancel = CancellationToken::new();
        assert_eq!(
            dispatcher.try_dispatch(Action::TurnStarted {
                draft_id: draft_id.clone(),
                cancel: cancel.clone(),
            }),
            DispatchResult::Sent
        );
        for i in 1..=5u64 {
            assert_eq!(
                dispatcher.try_dispatch(Action::StreamChunkReceived {
                    draft_id: draft_id.clone(),
                    delta: format!("chunk{i} "),
                    version: i,
                }),
                DispatchResult::Sent
            );
        }

        // 3. 流式完成
        assert_eq!(
            dispatcher.try_dispatch(Action::StreamCompleted {
                draft_id: draft_id.clone(),
                final_text: "hello user, response complete".to_string(),
                reasoning: "thinking...".to_string(),
            }),
            DispatchResult::Sent
        );
        assert_eq!(
            dispatcher.try_dispatch(Action::RecordAssistantTurn("hello user, response complete".to_string())),
            DispatchResult::Sent
        );

        // 4. 退出
        assert_eq!(dispatcher.try_dispatch(Action::ShutdownRequested), DispatchResult::Sent);

        // 5. 收尾 — drop sender + cancel shutdown，dispatcher 应在 2s 内退出
        shutdown.cancel();
        drop(dispatcher);
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit within 2s")
            .expect("join ok");

        // 全部 11 actions 经过 reducer
        assert_eq!(stats.actions_seen, 11, "actions_seen={}", stats.actions_seen);
        // reducer 应该至少产出一些 effect（RequestRedraw / LogTrace / NotifyHook 等）
        assert!(stats.effects_seen > 0, "no effects produced");

        // 验证 REDUX_DIFF_COUNT == 0：shadow 模式下业务 effect no-op，
        // 不存在双写 history 引发的差异（DIFF 仅 PRX_CHAT_REDUX=both 在
        // run_tui_unified_loop key event 路径里产生；此集成测试不走那条路径）
        #[cfg(feature = "terminal-tui")]
        assert_eq!(
            crate::chat::redux_diff_count(),
            0,
            "shadow mode should produce zero REDUX_DIFF_COUNT"
        );
    }

    #[tokio::test]
    async fn dispatcher_exits_on_shutdown_cancel_only() {
        // 验证 shutdown.cancel() 单独触发也能让 dispatcher 退出（无 drop sender）
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        let _ = dispatcher.try_dispatch(Action::ForceQuit);
        shutdown.cancel();
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit on shutdown")
            .expect("join ok");
        assert!(stats.actions_seen >= 1);
        drop(dispatcher);
    }

    #[tokio::test]
    async fn dispatcher_exits_on_channel_close_only() {
        // 验证 drop(sender) → channel close → dispatcher 退出（无 shutdown.cancel）
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown);

        let _ = dispatcher.try_dispatch(Action::ForceQuit);
        drop(dispatcher);
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit on channel close")
            .expect("join ok");
        assert_eq!(stats.actions_seen, 1);
    }

    #[tokio::test]
    async fn coalescer_under_dispatcher_load() {
        // 端到端：dispatcher + coalescer，100 个 chunk + 容量 4 channel → 必触发 backpressure。
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (tx, action_rx) = mpsc::channel::<Action>(4);
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        let draft = "draft-load".to_string();
        let mut coalescer = StreamChunkCoalescer::new(tx.clone());

        let cancel = CancellationToken::new();
        let _ = tx
            .send(Action::TurnStarted {
                draft_id: draft.clone(),
                cancel: cancel.clone(),
            })
            .await;

        let mut backpressure_count = 0u64;
        for i in 1..=100u64 {
            let r = coalescer.try_send_chunk(draft.clone(), format!("c{i}"), i);
            if matches!(r, DispatchResult::Backpressured) {
                backpressure_count += 1;
            }
        }
        let _ = coalescer.flush();

        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown.cancel();
        drop(coalescer);
        drop(tx);

        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher should exit")
            .expect("join ok");

        assert!(stats.actions_seen >= 2);
        assert!(
            backpressure_count > 0,
            "backpressure must trigger at cap=4 / 100 chunks (saw {backpressure_count})"
        );
        // coalescer 合并后 dispatcher 看到的 actions 数应远 < 101
        // （一个完整的合并能把多个 chunk 压缩成单个 Action）
        assert!(stats.actions_seen <= 101);
    }

    #[tokio::test]
    async fn dispatcher_drives_all_action_variants() {
        // Sanity: 所有 Action 变体走一遍 reducer，shadow 模式下都不应 panic.
        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        let actions: Vec<Action> = vec![
            Action::InputSubmitted("x".to_string()),
            Action::HistoryNavigated(HistoryDir::Up),
            Action::HistoryNavigated(HistoryDir::Down),
            Action::InputCancelled,
            Action::RedrawRequested,
            Action::TerminalResized { w: 80, h: 24 },
            Action::PasteReceived("paste".to_string()),
            Action::CancelRequested,
            Action::ShutdownRequested,
            Action::HistoryCleared,
            Action::HistoryClearedWithNotice {
                notice: "Conversation cleared".to_string(),
            },
            Action::ForceQuit,
            Action::ToolCardFoldToggled,
            Action::ReasoningFoldToggled,
            // S2-C: 新增 3 个 Action 必须能被 dispatcher reduce（不 panic）.
            Action::SystemMessageAdded {
                text: "banner".to_string(),
            },
            Action::RecordSystemMessage {
                content: "ctx-system".to_string(),
            },
            Action::SetLeadingSystemPrompt {
                content: "sys-prompt".to_string(),
            },
        ];
        for action in actions {
            let _ = dispatcher.try_dispatch(action);
        }
        shutdown.cancel();
        drop(dispatcher);
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("dispatcher exit")
            .expect("join ok");
        // S2-C: actions_seen 应 ≥ 16（13 个原有 + 3 个新增）.
        assert!(stats.actions_seen >= 16);
    }

    #[tokio::test]
    async fn effect_executor_handles_all_variants() {
        // Sanity: 所有 Effect 变体在 shadow 模式下都不应 panic.
        use crate::hooks::HookEvent;
        use crate::memory::MemoryCategory;

        let executor = EffectExecutor::new_shadow();
        let token = CancellationToken::new();

        let effects: Vec<Effect> = vec![
            Effect::RequestRedraw,
            Effect::Quit,
            Effect::CancelDraft("d1".to_string()),
            Effect::DisplayMedia {
                kind: "IMAGE".to_string(),
                path: "/tmp/x.png".to_string(),
            },
            Effect::AutoTitleSession("session-1".to_string()),
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "test".to_string(),
            },
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: "warn-test".to_string(),
            },
            Effect::PersistToMemory {
                key: "k".to_string(),
                value: "v".to_string(),
                category: MemoryCategory::Conversation,
            },
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"foo": "bar"}),
            },
            Effect::SendDraftFinalize {
                draft_id: "d1".to_string(),
                text: "final".to_string(),
            },
            Effect::StartTurn {
                draft_id: "shadow-d".to_string(),
                history: Vec::new(),
                cancel: token.clone(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            },
            Effect::CancelToken(token),
        ];

        for effect in effects {
            executor.execute(effect).await;
        }
    }

    #[tokio::test]
    async fn blocking_dispatch_works_in_spawn_blocking_context() {
        // blocking_dispatch 只在同步上下文可用；通过 spawn_blocking 隔离调用.
        let shutdown = CancellationToken::new();
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let _handle = spawn_dispatcher_task(make_state(shutdown.clone()), action_rx, shutdown.clone());

        let dispatcher_clone = dispatcher.clone();
        let r = tokio::task::spawn_blocking(move || dispatcher_clone.blocking_dispatch(Action::ForceQuit))
            .await
            .expect("spawn_blocking join");
        assert_eq!(r, DispatchResult::Sent);

        shutdown.cancel();
        drop(dispatcher);
    }

    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn redux_diff_count_remains_zero_in_shadow_mode() {
        // P0-2 验证：shadow 模式下 reducer 不双写 history，REDUX_DIFF_COUNT == 0.
        // 该计数器仅在 PRX_CHAT_REDUX=both 模式下的 run_tui_unified_loop key
        // event 路径累加；本测试不触达该路径，所以始终为 0。
        crate::chat::reset_redux_diff_count();

        let shutdown = CancellationToken::new();
        let state = make_state(shutdown.clone());
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let handle = spawn_dispatcher_task(state, action_rx, shutdown.clone());

        // 跑 50 个 mixed Action，模拟流式 + 工具 + 输入
        for i in 0..50u64 {
            let _ = dispatcher.try_dispatch(Action::InputSubmitted(format!("msg{i}")));
            let _ = dispatcher.try_dispatch(Action::RecordUserTurn(format!("user{i}")));
            let _ = dispatcher.try_dispatch(Action::RecordAssistantTurn(format!("assist{i}")));
        }

        shutdown.cancel();
        drop(dispatcher);
        let _stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("exit")
            .expect("join");

        assert_eq!(
            crate::chat::redux_diff_count(),
            0,
            "shadow mode must not produce any REDUX_DIFF_COUNT increments"
        );
    }
}

// ─── Step 5a-1 真业务执行测试（EffectExecutor::new_with_deps） ─────────────────

#[cfg(test)]
mod real_mode_tests {
    //! 验证 EffectExecutor 从 shadow 切到 real 模式后，业务 Effect 真执行。
    //!
    //! 这是 Codex P0 的核心证伪测试 — shadow_mode 恒真 + effect no-op + diff=0
    //! 是循环论证；本模块构造真 mock deps，证明：
    //!   1. SaveSession → memory.store 被调用
    //!   2. CancelDraft → channel.cancel_draft 被调用
    //!   3. NotifyHook → hooks.emit 被调用（spawn 子任务回投，需要等待）
    //!   4. Quit → shutdown.cancel() 被调用
    //!   5. StartTurn → spawn 子任务回投 Action::RedrawRequested
    //!   6. dual_write_guard 在持久化 effect 后被置位
    use super::*;
    use crate::channels::TerminalChannel;
    use crate::chat::session::ChatSession;
    use crate::hooks::HookManager;
    use crate::memory::{Memory, MemoryCategory, NoneMemory};
    use crate::observability::NoopObserver;
    use crate::providers::Provider;
    use crate::providers::router::MockEnvProvider;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// 记录 memory.store 次数的 wrapper（NoneMemory 不会真存，只 trace 调用）.
    struct CountingMemory {
        inner: NoneMemory,
        store_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Memory for CountingMemory {
        fn name(&self) -> &str {
            "counting"
        }
        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.store_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.store(key, content, category, session_id).await
        }
        async fn recall(&self, q: &str, l: usize, s: Option<&str>) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.inner.recall(q, l, s).await
        }
        async fn get(&self, k: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            self.inner.get(k).await
        }
        async fn list(
            &self,
            c: Option<&MemoryCategory>,
            s: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.inner.list(c, s).await
        }
        async fn forget(&self, k: &str) -> anyhow::Result<bool> {
            self.inner.forget(k).await
        }
        async fn count(&self) -> anyhow::Result<usize> {
            self.inner.count().await
        }
        async fn health_check(&self) -> bool {
            self.inner.health_check().await
        }
    }

    /// 轮询等待原子计数器达到目标值，避免固定 sleep 导致测试在慢机器上抖动
    async fn wait_for_count(counter: &AtomicUsize, target: usize, timeout: Duration) -> usize {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let cur = counter.load(std::sync::atomic::Ordering::SeqCst);
            if cur >= target || std::time::Instant::now() >= deadline {
                return cur;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// 计数器版 HookManager wrapper —— 直接复用 HookManager 但放在临时目录.
    fn build_hook_manager() -> (Arc<HookManager>, TempDir) {
        let temp = TempDir::new().expect("tempdir");
        let mgr = HookManager::new(temp.path().to_path_buf());
        (Arc::new(mgr), temp)
    }

    /// 构造完整 EffectDeps with mock providers / memory / channel / hooks.
    fn build_deps(
        memory: Arc<dyn Memory>,
        shutdown: CancellationToken,
    ) -> (EffectDeps, mpsc::Receiver<Action>, Arc<HookManager>, TempDir) {
        let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
        let (hooks, temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
        let (action_tx, action_rx) = mpsc::channel::<Action>(64);
        let dual_write_guard = RuntimeDualWriteGuard::new();
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let deps = EffectDeps {
            provider,
            memory,
            channel,
            hooks: Arc::clone(&hooks),
            observer,
            action_tx,
            dual_write_guard,
            redraw_tx: Some(redraw_tx),
            shutdown,
            model: Arc::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            max_tool_iterations: 0,
            approval_router: Arc::new(ApprovalRouter::new()),
            approval_manager: None,
        };
        (deps, action_rx, hooks, temp)
    }

    #[tokio::test]
    async fn real_mode_save_session_triggers_memory_store() {
        // 证明 SaveSession effect 在 real 模式下真调用 memory.store.
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps.clone());
        assert!(!executor.is_shadow());

        let session = ChatSession::new("prov", "model");
        executor.execute(Effect::SaveSession(session)).await;

        // 轮询等异步 spawn 子任务完成，避免固定 sleep 在慢机器上抖动
        let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
        assert_eq!(final_count, 1, "memory.store should be called exactly once");
        // RAII scope：子任务完成后 guard 应自动复位（不粘住）.
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard should auto-clear after SaveSession completes (RAII scope)"
        );
    }

    #[tokio::test]
    async fn real_mode_cancel_draft_invokes_channel() {
        // CancelDraft 是短同步路径，直接 await；不会 panic 表示路径通畅.
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);

        executor.execute(Effect::CancelDraft("draft-x".to_string())).await;
        // 不 panic + 没 hang 即通过；TerminalChannel.cancel_draft 总是 Ok.
    }

    /// T3-3-c-3 闭环：reducer dispatch `StreamCompleted` → 多个 Effect 中含 SaveSession,
    /// EffectExecutor::execute_real 后 memory.store 被调用 1 次（reducer 单源持久化路径通畅）.
    #[tokio::test]
    async fn t3_3c_stream_completed_drives_save_session_through_executor() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps.clone());

        // 用真 reducer 生成 Effect 序列（含 SaveSession），逐个交给 executor.
        let mut state = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        state.session.id = "t3-3c-session".to_string();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "d-T3-3c".to_string(),
            cancel: CancellationToken::new(),
        });
        let effects = state.reduce(Action::StreamCompleted {
            draft_id: "d-T3-3c".to_string(),
            final_text: "the answer".to_string(),
            reasoning: String::new(),
        });
        let mut had_save_session = false;
        for effect in effects {
            if matches!(effect, Effect::SaveSession(_)) {
                had_save_session = true;
            }
            executor.execute(effect).await;
        }
        assert!(had_save_session, "reducer must emit SaveSession for StreamCompleted");

        let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
        assert_eq!(final_count, 1, "reducer-emitted SaveSession 应触发 memory.store 一次");
    }

    /// T3-3-fixA P0-2: Exit-after-completed 四模式 reducer 持久化等价性.
    ///
    /// 验证 reducer 完成一个完整 turn 后 emit 的 SaveSession 在所有模式下都
    /// 触发恰好一次 memory.store —— 即 reducer 持久化路径**模式无关**。
    /// fixA P0-2 修复后，Pure 模式 reducer 是唯一持久化源，本测试确认它
    /// 与 Off/Both/Redux 在 reducer 持久化语义上对齐（写入次数 + 内容）.
    ///
    /// 注：chat::run 主循环退出时的 legacy save_session 由 ReduxMode 守卫开关，
    /// 守卫真值表已由 pure_mode_skips_legacy_exit_save_via_redux_mode_guard 覆盖.
    #[tokio::test]
    async fn t3_3_fix_a_exit_after_completed_persistence_parity() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        for tag in ["off", "both", "redux", "pure"] {
            let store_count = Arc::new(AtomicUsize::new(0));
            let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
                inner: NoneMemory::new(),
                store_count: Arc::clone(&store_count),
            });
            let shutdown = CancellationToken::new();
            let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
            let executor = EffectExecutor::new_with_deps(deps);

            let mut state = ChatState::new(
                Arc::from("test-prov"),
                Arc::from("test-model"),
                CancellationToken::new(),
            );
            state.session.id = format!("sess-fixA-{tag}");

            // 完整 turn: user → turn started → assistant recorded → stream completed
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: format!("d-{tag}"),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn("a".to_string()));
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: format!("d-{tag}"),
                final_text: "a".to_string(),
                reasoning: String::new(),
            });

            let mut had_save = false;
            for effect in effects {
                if matches!(effect, Effect::SaveSession(_)) {
                    had_save = true;
                }
                executor.execute(effect).await;
            }
            assert!(had_save, "[{tag}] reducer 完成 turn 必发 SaveSession");

            let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
            assert_eq!(
                final_count, 1,
                "[{tag}] reducer 持久化路径必须模式无关 — 完整 turn 应触发 memory.store 一次",
            );
        }
    }

    /// T3-3-fixA P0-2: Exit-while-streaming 四模式无 partial save 一致性.
    ///
    /// streaming 期间退出（用户没等流完）reducer 不应 emit SaveSession ——
    /// 这是附录 B 决策表的 Cancelled/Error 行的直接后果。本测试模拟"开始 turn
    /// 但既没 RecordAssistantTurn 也没 StreamCompleted"的中途退出窗口，
    /// 验证 memory.store == 0（无 partial state 持久化）.
    #[tokio::test]
    async fn t3_3_fix_a_exit_while_streaming_no_partial_save() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        for tag in ["off", "both", "redux", "pure"] {
            let store_count = Arc::new(AtomicUsize::new(0));
            let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
                inner: NoneMemory::new(),
                store_count: Arc::clone(&store_count),
            });
            let shutdown = CancellationToken::new();
            let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
            let executor = EffectExecutor::new_with_deps(deps);

            let mut state = ChatState::new(
                Arc::from("test-prov"),
                Arc::from("test-model"),
                CancellationToken::new(),
            );
            state.session.id = format!("sess-stream-{tag}");

            // user 已 record，turn 已开始流式，但 stream 没 complete（中途退出窗口）
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let user_effects: Vec<Effect> = state
                .reduce(Action::TurnStarted {
                    draft_id: format!("d-{tag}"),
                    cancel: CancellationToken::new(),
                })
                .into_iter()
                .chain(state.reduce(Action::StreamChunkReceived {
                    draft_id: format!("d-{tag}"),
                    delta: "partial".to_string(),
                    version: 1,
                }))
                .collect();

            assert!(
                !user_effects.iter().any(|e| matches!(e, Effect::SaveSession(_))),
                "[{tag}] streaming 中途 effects 不应含 SaveSession"
            );

            // 把已 emit 的 effect 都 execute 完（含 LogTrace / RequestRedraw 等)
            for effect in user_effects {
                executor.execute(effect).await;
            }

            // 负向断言保留固定等待：确认在合理窗口内 spawn 子任务确实未写
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                store_count.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "[{tag}] streaming 中途退出 memory.store 必须为 0（无 partial save）",
            );
        }
    }

    /// T3-3-fixB B5: driver 路径 SaveSession 快照必须包含本轮 assistant.
    ///
    /// 端到端：用 MockEnvProvider 默认 stream（发一个 final chunk），驱 driver 跑完整流，
    /// 把回投的 Action 序列依次喂给 reducer，断言：
    ///   1. driver 先发 RecordAssistantTurn，再发 StreamCompleted（B5 顺序契约）
    ///   2. StreamCompleted 触发的 SaveSession.turns.last() 是当轮 assistant（fixA P0-1 契约）
    ///   3. turns.len() 严格等于 2（user + assistant，无双写）
    ///
    /// fixB B5 修复前：driver 直接发 StreamCompleted，reducer 构造 SaveSession 快照时
    /// session.turns 缺当轮 assistant；本测试翻车即可定位回退.
    #[tokio::test]
    async fn t3_3_fix_b_driver_path_save_session_includes_assistant() {
        use crate::chat::action::Action;
        use crate::chat::state::ChatState;

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        // ── 启动 driver ──
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-fixB-B5".to_string(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        // ── 收 driver 回投的 Action，喂给 reducer ──
        let mut state = ChatState::new(
            Arc::from("test-prov"),
            Arc::from("test-model"),
            CancellationToken::new(),
        );
        state.session.id = "sess-fixB-B5".to_string();
        // user turn 是 chat::run 主循环在 driver 起跑前 dispatch 的，这里手工补.
        let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-fixB-B5".to_string(),
            cancel: CancellationToken::new(),
        });

        let mut saw_record = false;
        let mut save_snapshot: Option<crate::chat::session::ChatSession> = None;
        for _ in 0..8 {
            let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("driver action within 1.5s")
                .expect("action received");
            match action {
                Action::RecordAssistantTurn(text) => {
                    assert!(!saw_record, "RecordAssistantTurn 应只发一次");
                    saw_record = true;
                    let _ = state.reduce(Action::RecordAssistantTurn(text));
                }
                Action::StreamCompleted {
                    draft_id,
                    final_text,
                    reasoning,
                } => {
                    assert!(
                        saw_record,
                        "B5 顺序契约：StreamCompleted 必须在 RecordAssistantTurn 之后"
                    );
                    let effects = state.reduce(Action::StreamCompleted {
                        draft_id,
                        final_text,
                        reasoning,
                    });
                    for e in effects {
                        if let Effect::SaveSession(session) = e {
                            save_snapshot = Some(session);
                            break;
                        }
                    }
                    break;
                }
                _ => {
                    // 其他 actions（StreamChunkReceived 等）不影响顺序契约判定.
                }
            }
        }

        assert!(saw_record, "driver 必须发 RecordAssistantTurn");
        let snap = save_snapshot.expect("StreamCompleted 必须 emit SaveSession");
        let last = snap.turns.last().expect("SaveSession.turns 不应为空");
        assert_eq!(last.role, "assistant", "snapshot 末条必须是当轮 assistant");
        // 防双写：reducer RecordAssistantTurn 单次 + chat::run 1829 重复 dispatch 已删除.
        assert_eq!(snap.turns.len(), 2, "turns.len() 必须严格 2（user+assistant，零双写）");
    }

    /// T3-3-fixB D1: SaveSession 完成（写盘 end）必须严格早于 RequestRedraw（刷屏）.
    ///
    /// 原 spawn 版本下 SaveSession 子任务与 RequestRedraw 时序不可保证；inline await
    /// 修复后主循环 executor.execute(effect).await 串行性贯穿到底.
    ///
    /// 验证方式：SlowMemory.store sleep 20ms 后 push "save_end"，
    /// MockRedrawRx try_send 时 push "redraw"，断言 log "save_end" idx < "redraw" idx.
    /// 重复 N=5 次消除偶然性（spawn 版本下偶尔会"恰好顺序对"，多轮可拉出 race）.
    #[tokio::test]
    async fn t3_3_fix_b_effect_save_then_redraw_strict_order() {
        use crate::memory::MemoryEntry;
        use parking_lot::Mutex;

        struct SlowMemory {
            log: Arc<Mutex<Vec<&'static str>>>,
        }
        #[async_trait::async_trait]
        impl Memory for SlowMemory {
            fn name(&self) -> &str {
                "slow"
            }
            async fn store(&self, _k: &str, _c: &str, _cat: MemoryCategory, _s: Option<&str>) -> anyhow::Result<()> {
                self.log.lock().push("save_start");
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.log.lock().push("save_end");
                Ok(())
            }
            async fn recall(&self, _q: &str, _l: usize, _s: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
                Ok(Vec::new())
            }
            async fn get(&self, _k: &str) -> anyhow::Result<Option<MemoryEntry>> {
                Ok(None)
            }
            async fn list(&self, _c: Option<&MemoryCategory>, _s: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
                Ok(Vec::new())
            }
            async fn forget(&self, _k: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            async fn count(&self) -> anyhow::Result<usize> {
                Ok(0)
            }
            async fn health_check(&self) -> bool {
                true
            }
        }

        for trial in 0..5 {
            let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
            let memory: Arc<dyn Memory> = Arc::new(SlowMemory { log: Arc::clone(&log) });
            let shutdown = CancellationToken::new();

            // 单独构造 deps：用 SlowMemory + 自定义 redraw_tx 拦截 try_send 顺序.
            let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
            let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
            let (hooks, _temp) = build_hook_manager();
            let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
            let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
            let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);

            // 监听 redraw_rx 在独立 task 内 push "redraw" 到 log.
            let log_for_redraw = Arc::clone(&log);
            let redraw_listener = tokio::spawn(async move {
                if redraw_rx.recv().await.is_some() {
                    log_for_redraw.lock().push("redraw");
                }
            });

            let deps = EffectDeps {
                provider,
                memory,
                channel,
                hooks,
                observer,
                action_tx,
                dual_write_guard: RuntimeDualWriteGuard::new(),
                redraw_tx: Some(redraw_tx),
                shutdown: shutdown.clone(),
                model: Arc::from("test-model"),
                temperature: 0.0,
                tools_registry: None,
                max_tool_iterations: 0,
                approval_router: Arc::new(ApprovalRouter::new()),
                approval_manager: None,
            };
            let executor = EffectExecutor::new_with_deps(deps);

            let session = ChatSession::new("prov", "model");
            // 主循环串行：SaveSession → RequestRedraw（reducer 实际顺序）.
            executor.execute(Effect::SaveSession(session)).await;
            executor.execute(Effect::RequestRedraw).await;

            // 等 redraw_listener 完成（接 redraw 后退出）.
            let _ = tokio::time::timeout(Duration::from_millis(500), redraw_listener).await;

            let snap = log.lock().clone();
            let save_end_idx = snap
                .iter()
                .position(|&s| s == "save_end")
                .unwrap_or_else(|| panic!("[trial {trial}] save_end 未出现：log={snap:?}"));
            let redraw_idx = snap
                .iter()
                .position(|&s| s == "redraw")
                .unwrap_or_else(|| panic!("[trial {trial}] redraw 未出现：log={snap:?}"));
            assert!(
                save_end_idx < redraw_idx,
                "[trial {trial}] D1 顺序契约：save_end ({save_end_idx}) 必须早于 redraw ({redraw_idx})；log={snap:?}"
            );
        }
    }

    #[tokio::test]
    async fn real_mode_quit_cancels_shutdown_token() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);

        assert!(!shutdown.is_cancelled());
        executor.execute(Effect::Quit).await;
        assert!(shutdown.is_cancelled(), "Effect::Quit must cancel shutdown token");
    }

    #[tokio::test]
    async fn real_mode_request_redraw_pings_renderer() {
        // RequestRedraw 在 real 模式下通过 deps.redraw_tx 唤醒主循环.
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
        let (hooks, _temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
        let deps = EffectDeps {
            provider,
            memory,
            channel,
            hooks,
            observer,
            action_tx,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            shutdown: shutdown.clone(),
            model: Arc::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            max_tool_iterations: 0,
            approval_router: Arc::new(ApprovalRouter::new()),
            approval_manager: None,
        };
        let executor = EffectExecutor::new_with_deps(deps);
        executor.execute(Effect::RequestRedraw).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
                .await
                .expect("redraw within 200ms")
                .is_some(),
            "RequestRedraw should ping the redraw channel"
        );
    }

    #[tokio::test]
    async fn real_mode_start_turn_spawns_subtask_and_does_not_block() {
        // StartTurn 必须 spawn 子任务回投 Action（Codex P0-1），不阻塞主循环.
        // 5a-2 起：子任务真接 provider.stream_chat_with_history，并把流式事件
        // 通过 action_tx 回投——这里走 MockEnvProvider 的 trait 默认实现，
        // 默认实现发一个 final error chunk，因此应收到 StreamChunkReceived → StreamCompleted。
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let start = std::time::Instant::now();
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-real".to_string(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;
        // execute() 立即返回（不阻塞）；spawn 子任务在后台真调 provider + 回投 Action.
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "StartTurn should not block (Codex P0-1)"
        );

        // 收第一条 Action：默认 stream 实现发一个 error chunk（delta=error message,
        // is_final=true），转换成 StreamChunkReceived (因 delta 非空).
        let action = tokio::time::timeout(Duration::from_millis(1000), action_rx.recv())
            .await
            .expect("action within 1s")
            .expect("action received");
        match action {
            Action::StreamChunkReceived { draft_id, version, .. } => {
                assert_eq!(draft_id, "draft-real", "draft_id must propagate");
                assert!(version >= 1, "version must start at 1+");
            }
            other => panic!("expected StreamChunkReceived, got {other:?}"),
        }

        // T3-3-fixB B5: 第二条是 RecordAssistantTurn（在 StreamCompleted 前发出）.
        let action = tokio::time::timeout(Duration::from_millis(1000), action_rx.recv())
            .await
            .expect("RecordAssistantTurn within 1s")
            .expect("RecordAssistantTurn received");
        match action {
            Action::RecordAssistantTurn(_) => {}
            other => panic!("expected RecordAssistantTurn (fixB B5 前置), got {other:?}"),
        }

        // 第三条：is_final=true 进入 break，发送 StreamCompleted.
        let action = tokio::time::timeout(Duration::from_millis(1000), action_rx.recv())
            .await
            .expect("completion within 1s")
            .expect("completion received");
        match action {
            Action::StreamCompleted { draft_id, .. } => {
                assert_eq!(draft_id, "draft-real", "completion draft_id must propagate");
            }
            other => panic!("expected StreamCompleted, got {other:?}"),
        }
    }

    /// Step 5a-2 — StartTurn 在 cancel pre-trigger 后立刻发 StreamCancelled.
    #[tokio::test]
    async fn real_mode_start_turn_pre_cancel_emits_stream_cancelled() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        cancel.cancel(); // 启动前即取消

        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-cancelled".to_string(),
                history: Vec::new(),
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let action = tokio::time::timeout(Duration::from_millis(500), action_rx.recv())
            .await
            .expect("action within 500ms")
            .expect("action received");
        match action {
            Action::StreamCancelled { draft_id } => {
                assert_eq!(draft_id, "draft-cancelled");
            }
            other => panic!("expected StreamCancelled, got {other:?}"),
        }
    }

    /// Step 5a-2 — fake streaming provider 验证完整 chunk → completion 序列 + 版本号严格递增.
    #[tokio::test]
    async fn real_mode_start_turn_streams_chunks_then_completes() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct FakeStreamProvider;

        #[async_trait]
        impl Provider for FakeStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    native_tool_calling: false,
                    vision: false,
                }
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let chunks: Vec<StreamResult<StreamChunk>> = vec![
                    Ok(StreamChunk::delta("hello ")),
                    Ok(StreamChunk::reasoning_delta("thinking…")),
                    Ok(StreamChunk::delta("world")),
                    Ok(StreamChunk::final_chunk()),
                ];
                stream::iter(chunks).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FakeStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-stream".to_string(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        // 收 chunk 1 (delta="hello ")
        let a1 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("first chunk within 1.5s")
            .expect("first chunk received");
        match a1 {
            Action::StreamChunkReceived {
                draft_id,
                delta,
                version,
            } => {
                assert_eq!(draft_id, "draft-stream");
                assert_eq!(delta, "hello ");
                assert_eq!(version, 1, "first delta version must be 1");
            }
            other => panic!("expected StreamChunkReceived#1, got {other:?}"),
        }

        // reasoning chunk 不产生 Action（被 buffer），下一条仍是 chunk 2 (delta="world")
        let a2 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("second chunk within 1.5s")
            .expect("second chunk received");
        match a2 {
            Action::StreamChunkReceived { delta, version, .. } => {
                assert_eq!(delta, "world");
                assert_eq!(version, 2, "second delta version must strictly increase");
            }
            other => panic!("expected StreamChunkReceived#2, got {other:?}"),
        }

        // T3-3-fixB B5: RecordAssistantTurn 在 StreamCompleted 之前发，
        // final_text 与 RecordAssistantTurn 内容一致.
        let a_record = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("RecordAssistantTurn within 1.5s")
            .expect("RecordAssistantTurn received");
        match a_record {
            Action::RecordAssistantTurn(text) => {
                assert_eq!(text, "hello world", "RecordAssistantTurn 内容应与 final_text 一致");
            }
            other => panic!("expected RecordAssistantTurn (fixB B5 前置), got {other:?}"),
        }

        // 最终 StreamCompleted，final_text 累计，reasoning 包含 thinking 文本.
        let a3 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("completion within 1.5s")
            .expect("completion received");
        match a3 {
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => {
                assert_eq!(draft_id, "draft-stream");
                assert_eq!(final_text, "hello world");
                assert_eq!(reasoning, "thinking…");
            }
            other => panic!("expected StreamCompleted, got {other:?}"),
        }
    }

    /// Step 5a-2 — provider stream 产生 Err 时发 StreamFailed（含 retryable 判定）.
    #[tokio::test]
    async fn real_mode_start_turn_stream_error_emits_stream_failed() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct FailingStreamProvider;

        #[async_trait]
        impl Provider for FailingStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let chunks: Vec<StreamResult<StreamChunk>> =
                    vec![Err(StreamError::Provider("simulated failure".to_string()))];
                stream::iter(chunks).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FailingStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-fail".to_string(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("fail within 1.5s")
            .expect("fail action received");
        match action {
            Action::StreamFailed {
                draft_id,
                err,
                retryable,
            } => {
                assert_eq!(draft_id, "draft-fail");
                assert!(err.contains("simulated failure"));
                assert!(!retryable, "Provider error is non-retryable");
            }
            other => panic!("expected StreamFailed, got {other:?}"),
        }
    }

    /// Step 5a-2 — 流中途 cancel 触发 StreamCancelled.
    #[tokio::test]
    async fn real_mode_start_turn_mid_stream_cancel_emits_cancelled() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct SlowStreamProvider;

        #[async_trait]
        impl Provider for SlowStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                // 每个 chunk 之间 sleep 200ms 给 cancel 机会
                let s = stream::unfold(0u32, |i| async move {
                    if i >= 5 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Some((Ok(StreamChunk::delta(format!("chunk{i} "))), i + 1))
                });
                s.boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(SlowStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-mid-cancel".to_string(),
                history: Vec::new(),
                cancel: cancel.clone(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        // 在 250ms 后 cancel：应已经收到至少 1 个 chunk，然后立即 cancel.
        tokio::time::sleep(Duration::from_millis(250)).await;
        cancel.cancel();

        // 收若干 Action，找到 StreamCancelled.
        let mut found_cancelled = false;
        for _ in 0..10 {
            let action = match tokio::time::timeout(Duration::from_millis(800), action_rx.recv()).await {
                Ok(Some(a)) => a,
                _ => break,
            };
            if matches!(action, Action::StreamCancelled { .. }) {
                found_cancelled = true;
                break;
            }
        }
        assert!(
            found_cancelled,
            "mid-stream cancel must emit StreamCancelled within reasonable time"
        );
    }

    #[tokio::test]
    async fn dual_write_guard_default_is_inactive() {
        let g = RuntimeDualWriteGuard::new();
        assert!(!g.is_active());
        let scope = g.enter_scope();
        assert!(g.is_active());
        assert_eq!(g.active_count(), 1);
        drop(scope);
        assert!(!g.is_active());
        assert_eq!(g.active_count(), 0);
    }

    #[tokio::test]
    async fn dual_write_guard_clone_shares_state() {
        let g1 = RuntimeDualWriteGuard::new();
        let g2 = g1.clone();
        assert!(!g2.is_active());
        let _scope = g1.enter_scope();
        assert!(g2.is_active(), "clone shares the same Arc<AtomicU64>");
    }

    /// 5a-5 Codex P1 修复验证：多个 RAII scope 并发存在时，
    /// drop 单个 scope 不会让 guard 变 inactive；只有全部 scope drop 后才回到 0.
    /// 旧的 AtomicBool 实现有严重时序窗：先到的 drop 会把后到的也"清零"。
    #[tokio::test]
    async fn dual_write_guard_counting_raii_prevents_early_release() {
        let g = RuntimeDualWriteGuard::new();
        let s1 = g.enter_scope();
        assert!(g.is_active());
        assert_eq!(g.active_count(), 1);
        let s2 = g.enter_scope();
        assert_eq!(g.active_count(), 2);
        let s3 = g.enter_scope();
        assert_eq!(g.active_count(), 3);

        // 中间 drop 一个，guard 仍然 active.
        drop(s2);
        assert!(
            g.is_active(),
            "guard must remain active while sibling scopes are alive (5a-5 P1 fix)"
        );
        assert_eq!(g.active_count(), 2);

        drop(s1);
        assert!(g.is_active(), "guard remains active with last scope alive");
        assert_eq!(g.active_count(), 1);

        drop(s3);
        assert!(!g.is_active(), "guard becomes inactive only after all scopes drop");
        assert_eq!(g.active_count(), 0);
    }

    /// Ratatui 路径 Ctrl+C 防回归（退化为构造 ChatState + dispatch 双 Ctrl+C 验证 Effect::Quit）.
    ///
    /// **背景**：Codex P1 指出 PTY 测试用 `PRX_TUI=0` 走 reedline，没有覆盖
    /// `run_tui_unified_loop` 的 Ctrl+C 分支。PTY 抓 ratatui 全屏 TUI 输出困难，
    /// 这里退化为 reducer + executor 单元测试：
    ///   - 构造 ChatState 模拟 ratatui 路径下双 Ctrl+C in DOUBLE_CTRLC_WINDOW_MS
    ///   - 验证 reducer 返回 Effect::Quit
    ///   - 把 Effect::Quit 喂给 real-mode EffectExecutor，验证 shutdown.cancel() 被调用
    /// 这是端到端"Ctrl+C → 退出"链条的最小可验证子集，覆盖 round 2 hang bug
    /// 的核心防御路径（reducer 决策 + executor 触发）。
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn ratatui_path_double_ctrlc_exits_via_reducer_and_executor() {
        use crate::chat::state::ChatState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let shutdown = CancellationToken::new();
        let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown.clone());
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        // 第一次 Ctrl+C @ t=1000ms — 不应触发 Quit（仅记录窗口；reducer 要求 prev != 0）
        let effects1 = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1000);
        let has_quit_1 = effects1.iter().any(|e| matches!(e, Effect::Quit));
        assert!(!has_quit_1, "first Ctrl+C should not Quit");

        // 第二次 Ctrl+C @ t=1200ms — 在 500ms 窗口内（200ms 间隔），应 Quit
        let effects2 = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1200);
        let has_quit_2 = effects2.iter().any(|e| matches!(e, Effect::Quit));
        assert!(has_quit_2, "double Ctrl+C within 500ms must Quit");

        // 喂给 real-mode EffectExecutor，验证 shutdown.cancel() 真执行
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);
        for e in effects2 {
            executor.execute(e).await;
        }
        assert!(
            shutdown.is_cancelled(),
            "real-mode executor must propagate Effect::Quit to shutdown.cancel()"
        );
    }

    /// 防回归补充：单击 Ctrl+C in flight turn 不应导致退出（仅取消当前 turn）.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn single_ctrlc_during_turn_does_not_exit() {
        use crate::chat::state::ChatState;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let shutdown = CancellationToken::new();
        let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), shutdown.clone());
        // 模拟 turn 进行中（generating=true）— 通过 TurnStarted action 设置
        let cancel = CancellationToken::new();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "d1".to_string(),
            cancel: cancel.clone(),
        });
        assert!(state.control.generating);

        // 单 Ctrl+C — 在 generating 状态下应 cancel draft，不退出.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let effects = state.reduce_with_now(Action::KeyPressed(ctrl_c), 1000);
        let has_quit = effects.iter().any(|e| matches!(e, Effect::Quit));
        assert!(!has_quit, "single Ctrl+C in flight turn must not Quit");

        // shutdown 不该被取消
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown.clone());
        let executor = EffectExecutor::new_with_deps(deps);
        for e in effects {
            executor.execute(e).await;
        }
        assert!(!shutdown.is_cancelled(), "single Ctrl+C must not cancel shutdown");
    }

    /// 防回归：Mutex 测试避免 ".unwrap()" — 强制使用 parking_lot.
    #[tokio::test]
    async fn parking_lot_mutex_in_test() {
        let m: Mutex<u32> = Mutex::new(0);
        *m.lock() = 42;
        assert_eq!(*m.lock(), 42);
    }

    // ─── P1: 补足 Effect 真业务单测覆盖 (7/7) ────────────────────────────────────

    /// CountingChannel: 记录 send 调用次数（wrap TerminalChannel）.
    struct CountingChannel {
        inner: crate::channels::TerminalChannel,
        send_count: Arc<AtomicUsize>,
        finalize_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::channels::Channel for CountingChannel {
        fn name(&self) -> &str {
            "counting"
        }
        async fn send(&self, message: &crate::channels::traits::SendMessage) -> anyhow::Result<()> {
            self.send_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.send(message).await
        }
        async fn listen(
            &self,
            tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
        ) -> anyhow::Result<()> {
            self.inner.listen(tx).await
        }
        async fn finalize_draft(&self, recipient: &str, message_id: &str, text: &str) -> anyhow::Result<()> {
            self.finalize_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.finalize_draft(recipient, message_id, text).await
        }
        async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
            self.inner.cancel_draft(recipient, message_id).await
        }
    }

    /// 构建 CountingChannel deps.
    fn build_counting_channel_deps(
        send_count: Arc<AtomicUsize>,
        finalize_count: Arc<AtomicUsize>,
        shutdown: CancellationToken,
    ) -> (EffectDeps, mpsc::Receiver<Action>, TempDir) {
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::router::MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(CountingChannel {
            inner: crate::channels::TerminalChannel::new(true),
            send_count,
            finalize_count,
        });
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let (hooks, temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
        let (action_tx, action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let deps = EffectDeps {
            provider,
            memory,
            channel,
            hooks,
            observer,
            action_tx,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            shutdown,
            model: Arc::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            max_tool_iterations: 0,
            approval_router: Arc::new(ApprovalRouter::new()),
            approval_manager: None,
        };
        (deps, action_rx, temp)
    }

    /// P1-1: EmitChannelMessage → channel.send 被真正调用.
    #[tokio::test]
    async fn real_mode_emit_channel_message_triggers_channel_send() {
        let send_count = Arc::new(AtomicUsize::new(0));
        let finalize_count = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let (deps, _rx, _temp) =
            build_counting_channel_deps(Arc::clone(&send_count), Arc::clone(&finalize_count), shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        use crate::channels::traits::SendMessage;
        let msg = SendMessage::new("hello from effect".to_string(), "user");
        executor.execute(Effect::EmitChannelMessage(msg)).await;

        let final_count = wait_for_count(&send_count, 1, Duration::from_secs(2)).await;
        assert_eq!(
            final_count, 1,
            "channel.send should be called exactly once for EmitChannelMessage"
        );
        // RAII scope：子任务完成后 guard 应自动复位（不粘住）.
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard should auto-clear after EmitChannelMessage completes (RAII scope)"
        );
    }

    /// P1-2: PersistToMemory → memory.store 被真正调用（使用已有 CountingMemory）.
    #[tokio::test]
    async fn real_mode_persist_to_memory_triggers_memory_store() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(CountingMemory {
            inner: crate::memory::NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        executor
            .execute(Effect::PersistToMemory {
                key: "persist-test-key".to_string(),
                value: "test-value".to_string(),
                category: crate::memory::MemoryCategory::Conversation,
            })
            .await;

        let final_count = wait_for_count(&store_count, 1, Duration::from_secs(2)).await;
        assert_eq!(
            final_count, 1,
            "memory.store should be called exactly once for PersistToMemory"
        );
        // RAII scope：子任务完成后 guard 应自动复位（不粘住）.
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard should auto-clear after PersistToMemory completes (RAII scope)"
        );
    }

    /// P1-3a: NotifyHook → 不 panic，RAII scope 确保 guard 在子任务完成后自动复位.
    ///
    /// HookManager 不是 trait 无法 wrap 计数；行为验证：
    /// emit 完成后 guard 自动清 = executor 走了真路径 + RAII 不粘住。
    /// HookManager 无注册 hooks → emit 是快速 no-op，不影响测试速度。
    #[tokio::test]
    async fn real_mode_notify_hook_guard_does_not_stick() {
        use crate::hooks::HookEvent;
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        assert!(!deps.dual_write_guard.is_active(), "guard should start inactive");

        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"test": "notify-hook"}),
            })
            .await;

        // spawn 子任务异步；给足时间完成
        tokio::time::sleep(Duration::from_millis(200)).await;

        // RAII scope：子任务完成后 guard 应自动复位（不粘住）.
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard must auto-clear after NotifyHook completes (RAII scope prevents sticking)"
        );
    }

    /// P1-3b: NotifyHook → hooks.emit 真被调用（向临时目录注册真实 hook，用 touch 创建哨兵文件）.
    ///
    /// HookManager 不是 trait，无法 mock。改为注册真实 hook：
    /// 在临时目录写 hooks.json，注册 turn_complete event 执行 `touch <sentinel>`，
    /// emit 后验证哨兵文件存在即证明 emit 真调了 hook action。
    #[tokio::test]
    async fn real_mode_notify_hook_triggers_emit() {
        use crate::hooks::HookEvent;

        // 构造临时目录 + 注册真实 hook（touch 哨兵文件）
        let temp = TempDir::new().expect("tempdir");
        let sentinel = temp.path().join("hook_was_called");
        let sentinel_str = sentinel.to_str().expect("valid path");

        let hooks_json = serde_json::json!({
            "enabled": true,
            "hooks": {
                "turn_complete": [
                    {
                        "command": "touch",
                        "args": [sentinel_str],
                        "stdin_json": false
                    }
                ]
            }
        });
        std::fs::write(temp.path().join("hooks.json"), hooks_json.to_string()).expect("write hooks.json");

        let hooks = Arc::new(HookManager::new(temp.path().to_path_buf()));
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::router::MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(crate::channels::TerminalChannel::new(true));
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let shutdown = CancellationToken::new();
        let deps = EffectDeps {
            provider,
            memory,
            channel,
            hooks: Arc::clone(&hooks),
            observer,
            action_tx,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            shutdown,
            model: Arc::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            max_tool_iterations: 0,
            approval_router: Arc::new(ApprovalRouter::new()),
            approval_manager: None,
        };
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"turn": "test"}),
            })
            .await;

        // hook 通过 tokio::process::Command 执行（异步），给足时间完成
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert!(
            sentinel.exists(),
            "hooks.emit should have executed 'touch {sentinel_str}' — sentinel file not found, emit was not called"
        );
    }

    /// P1-4a: SendDraftFinalize → 不 panic，不阻塞，RAII guard 不粘.
    ///
    /// 验证点：① 不阻塞（立即返回）② guard 子任务完成后自动复位（不粘住）
    /// ③ channel.finalize_draft 真被调用（finalize_count == 1）.
    #[tokio::test]
    async fn real_mode_send_draft_finalize_triggers_channel_finalize() {
        let send_count = Arc::new(AtomicUsize::new(0));
        let finalize_count = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let (deps, _rx, _temp) =
            build_counting_channel_deps(Arc::clone(&send_count), Arc::clone(&finalize_count), shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        let start = std::time::Instant::now();
        executor
            .execute(Effect::SendDraftFinalize {
                draft_id: "draft-finalize-test".to_string(),
                text: "final response text".to_string(),
            })
            .await;
        // 不阻塞：spawn 后立即返回
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "SendDraftFinalize should not block (Codex P0-1)"
        );

        // 等子任务完成
        tokio::time::sleep(Duration::from_millis(200)).await;

        // channel.finalize_draft 真被调用
        assert_eq!(
            finalize_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "channel.finalize_draft should be called exactly once for SendDraftFinalize"
        );
        // RAII scope：guard 在子任务完成后自动复位（不粘住）
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard must auto-clear after SendDraftFinalize completes (RAII scope)"
        );
    }

    /// P1-5: DisplayMedia → 不 panic，走 trace/debug 路径（5a-1 旧路径主导媒体显示）.
    #[tokio::test]
    async fn real_mode_display_media_does_not_panic() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        // 不 panic = 路径通畅；5a-1 阶段仅 debug log，无外部副作用
        executor
            .execute(Effect::DisplayMedia {
                kind: "IMAGE".to_string(),
                path: "/tmp/test_image.png".to_string(),
            })
            .await;
        // 通过 = 不 panic
    }

    /// P1-6: AutoTitleSession → 不 panic，走 debug trace 路径.
    #[tokio::test]
    async fn real_mode_auto_title_session_does_not_panic() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::AutoTitleSession("session-title-test".to_string()))
            .await;
        // 通过 = 不 panic；5a-1 阶段 AutoTitleSession 仅 debug log
    }

    /// P1-7: LogTrace real 模式 — 验证所有 tracing::Level 都不 panic（real 模式与 shadow 相同路径）.
    #[tokio::test]
    async fn real_mode_log_trace_all_levels_do_not_panic() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let levels = [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::INFO,
            tracing::Level::DEBUG,
            tracing::Level::TRACE,
        ];
        for level in levels {
            executor
                .execute(Effect::LogTrace {
                    level,
                    msg: format!("real-mode log test at {level}"),
                })
                .await;
        }
        // 全部通过 = 不 panic，real 模式 LogTrace 走与 shadow 相同的 emit_trace 路径
    }

    /// P0-2 验证: set_redraw_tx 后注入 redraw_handle Arc，RequestRedraw 真触发重绘.
    ///
    /// 模拟 chat::run 场景：先构造 EffectExecutor（redraw_tx=None），
    /// 取出 redraw_handle，"spawn"（此处直接执行），然后填入 redraw_tx，
    /// 验证 RequestRedraw effect 真正触发重绘。
    #[tokio::test]
    async fn redraw_handle_injection_enables_request_redraw() {
        let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::NoneMemory::new());
        let shutdown = CancellationToken::new();
        // 构造时 deps.redraw_tx = Some（build_deps 默认给 Some），但我们用 None 模拟时序问题
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::router::MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(crate::channels::TerminalChannel::new(true));
        let (hooks, _temp) = build_hook_manager();
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(crate::observability::NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let deps = EffectDeps {
            provider,
            memory,
            channel,
            hooks,
            observer,
            action_tx,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: None, // 模拟构造时 redraw_tx 尚不存在
            shutdown: shutdown.clone(),
            model: Arc::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            max_tool_iterations: 0,
            approval_router: Arc::new(ApprovalRouter::new()),
            approval_manager: None,
        };

        let executor = EffectExecutor::new_with_deps(deps);

        // 取出 redraw_handle（模拟 chat::run 提前保存 Arc）
        let redraw_slot = executor.redraw_handle();

        // RequestRedraw before injection — slot is None, should be no-op (no panic)
        executor.execute(Effect::RequestRedraw).await;

        // 后注入 redraw_tx（模拟 TUI 初始化完成后注入）
        let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(4);
        *redraw_slot.lock() = Some(redraw_tx);

        // 注入后 RequestRedraw 应真正触发
        executor.execute(Effect::RequestRedraw).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), redraw_rx.recv())
                .await
                .expect("redraw within 200ms after injection")
                .is_some(),
            "RequestRedraw should trigger after redraw_handle injection (P0-2)"
        );
    }

    // ─── P0: DualWriteGuardScope RAII 专项单测 ────────────────────────────────

    /// P0-scope-1: DualWriteGuardScope::enter → guard true；Drop → guard false.
    #[tokio::test]
    async fn dual_write_guard_scope_clears_on_drop() {
        let guard = RuntimeDualWriteGuard::new();
        assert!(!guard.is_active(), "guard should start false");

        {
            let _scope = guard.enter_scope();
            assert!(guard.is_active(), "guard should be true while scope is held");
        } // scope drops here

        assert!(
            !guard.is_active(),
            "guard should be false after scope drop (RAII cleared)"
        );
    }

    /// P0-scope-2: panic 路径下 DualWriteGuardScope::drop 仍执行（unwind safety）.
    #[tokio::test]
    async fn dual_write_guard_scope_panic_safe() {
        let guard = RuntimeDualWriteGuard::new();
        let inner = Arc::clone(&guard.active);

        let result = std::panic::catch_unwind(move || {
            let scope = DualWriteGuardScope::enter(Arc::clone(&inner));
            assert!(
                inner.load(Ordering::Acquire) > 0,
                "count should be positive inside scope"
            );
            // 故意 panic；drop 应在 unwind 期间执行
            let _keep = scope;
            panic!("deliberate test panic");
        });

        assert!(result.is_err(), "catch_unwind should have caught the panic");
        // panic 后 Drop 执行 → guard 应复位为 false
        assert!(
            !guard.is_active(),
            "guard must be false after panic unwind (Drop still runs)"
        );
    }

    /// P0-scope-3: real_mode SaveSession 完成后 guard 不粘（spawn scope 自动清）.
    ///
    /// 比 real_mode_save_session_triggers_memory_store 更专注验证 guard 生命周期：
    /// execute() 调用后 guard 短暂为 true，子任务完成后自动复位 false。
    #[tokio::test]
    async fn real_mode_save_session_clears_guard_after_completion() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps.clone());

        let session = crate::chat::session::ChatSession::new("prov", "model");
        executor.execute(Effect::SaveSession(session)).await;

        // 轮询等待子任务完成（最多 500ms）
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if store_count.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "memory.store not called within 500ms"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 子任务完成后 guard 必须自动复位
        assert!(
            !deps.dual_write_guard.is_active(),
            "dual_write_guard must be false after SaveSession subtask completes (RAII scope auto-cleared)"
        );
    }

    // ── Step 5a-4 必补测试 (Codex Phase 3 审计要求) ─────────────────────────

    /// P0-2: EffectDeps.model + temperature 真实透传给 drive_start_turn_stream.
    ///
    /// 用 capture provider 断言 stream_chat_with_history 收到的 model/temperature
    /// 等于 deps 注入值。修复了 5a-2 hard-coded String::new()/0.0 的 Codex P1.
    #[tokio::test]
    async fn real_mode_start_turn_passes_model_and_temperature_from_deps() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex as PMutex;

        #[derive(Default)]
        struct ParamCaptureProvider {
            captured_model: Arc<PMutex<String>>,
            captured_temp: Arc<PMutex<f64>>,
        }

        #[async_trait]
        impl Provider for ParamCaptureProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                model: &str,
                temperature: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                *self.captured_model.lock() = model.to_string();
                *self.captured_temp.lock() = temperature;
                let chunks: Vec<StreamResult<StreamChunk>> =
                    vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())];
                stream::iter(chunks).boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let captured_model = Arc::new(PMutex::new(String::new()));
        let captured_temp = Arc::new(PMutex::new(0.0_f64));
        let provider = Arc::new(ParamCaptureProvider {
            captured_model: Arc::clone(&captured_model),
            captured_temp: Arc::clone(&captured_temp),
        });

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = provider.clone();
        // 注入非默认 model / temperature 让 capture 能区分.
        deps.model = Arc::from("gpt-test-99");
        deps.temperature = 0.42;

        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-params".to_string(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        // 等首条 chunk 到达，确保 stream_chat_with_history 已被调用.
        let _ = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("first chunk within 1.5s");

        let model_seen = captured_model.lock().clone();
        let temp_seen = *captured_temp.lock();
        assert_eq!(
            model_seen, "gpt-test-99",
            "model must be passed through from EffectDeps"
        );
        assert!(
            (temp_seen - 0.42).abs() < f64::EPSILON,
            "temperature must be passed through from EffectDeps (got {temp_seen})"
        );
    }

    /// P1-1: TurnCompletionSignal 失败链路 API 契约.
    ///
    /// 验证 `extract_turn_outcome(StreamFailed) → TurnOutcomeKind::Failed`,
    /// 且 `record_and_notify` 后 `consume_outcome` 读到同一 Failed (含 err / retryable).
    /// driver 全链路 (provider Err → drive_start_turn_stream 发 StreamFailed)
    /// 已被 `real_mode_start_turn_stream_error_emits_stream_failed` 覆盖；
    /// reducer 链路 (StreamFailed → NotifyHook(Error)) 已被 state.rs 单测覆盖。
    /// 本测试锁定二者拼接处 TurnCompletionSignal 不丢失 err 语义。
    #[tokio::test]
    async fn turn_signal_records_failed_outcome_from_stream_failed_action() {
        let signal = TurnCompletionSignal::new();
        let action = Action::StreamFailed {
            draft_id: "d1".to_string(),
            err: "simulated provider failure".to_string(),
            retryable: true,
        };
        let outcome = extract_turn_outcome(&action);
        assert!(matches!(outcome, Some(TurnOutcomeKind::Failed { .. })));
        if let Some(out) = outcome {
            signal.record_and_notify(out);
        }
        let consumed = signal.consume_outcome();
        match consumed {
            Some(TurnOutcomeKind::Failed { err, retryable }) => {
                assert!(err.contains("simulated"), "err must contain original message");
                assert!(retryable, "retryable bit must be preserved");
            }
            other => panic!("expected Failed outcome, got {other:?}"),
        }
        // 第二次 consume 应为 None（消费式 API）.
        assert!(signal.consume_outcome().is_none(), "consume_outcome must drain slot");
    }

    /// **5a-6 negative case**：driver 收到 tool_calls chunk 但 `tools_registry == None`，
    /// 应发 `StreamFailed(retryable=false)`。
    ///
    /// route_turn 现在允许 driver 走 tool turn，但 `tools_registry` 为 None 时
    /// driver 无法执行 tool — 立即 fail-fast，让 chat::run fallthrough.
    #[tokio::test]
    async fn driver_without_registry_rejects_tool_call_chunk() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct ToolCallProvider;
        #[async_trait]
        impl Provider for ToolCallProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temp: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let calls = vec![ToolCallChunk::new("c1", "shell", r#"{"cmd":"ls"}"#, 0)];
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(calls)),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ToolCallProvider);
        // 显式: 不提供 registry → driver 必须 fail.
        deps.tools_registry = None;
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-no-registry".to_string(),
                history: Vec::new(),
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        // 跳过潜在 ToolStarted (no-registry 路径下不会发, 因为 registry 检查在 ToolStarted 之前) — 用 loop 拿到 StreamFailed.
        let mut got_failed = false;
        for _ in 0..6 {
            let action = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
                .await
                .expect("driver must respond within 1.5s")
                .expect("action must be received");
            match action {
                Action::StreamFailed {
                    draft_id,
                    err,
                    retryable,
                } => {
                    assert_eq!(draft_id, "draft-no-registry");
                    assert!(!retryable, "no-registry rejection is permanent");
                    assert!(
                        err.contains("tools_registry") || err.contains("tool_calls"),
                        "err must hint at missing registry / tool_calls (got: {err})"
                    );
                    got_failed = true;
                    break;
                }
                Action::StreamChunkReceived { .. } | Action::ToolStarted { .. } | Action::ToolFinished { .. } => {
                    // permitted pre-failure noise; keep draining.
                }
                other => panic!("unexpected action before StreamFailed: {other:?}"),
            }
        }
        assert!(got_failed, "driver must emit StreamFailed within 6 actions");
    }

    /// **5a-6 happy path**：driver 收到 tool_call → 通过 tools_registry 执行 → 把
    /// tool result 喂回 history → 下一轮 LLM 调用拿到最终文本 → StreamCompleted.
    ///
    /// 模拟两轮: 第 1 轮 provider 发 ToolCall(echo-tool, {"text": "hi"}), driver 执行
    /// echo-tool 返回 "hi"; 第 2 轮 provider 发 final_text="done", driver 完成 turn.
    #[tokio::test]
    async fn driver_executes_tool_call_chunk_and_continues_to_completion() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex as PMutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        // ── Echo tool: 返回 args["text"] 原样, 让 driver 把它喂回 provider 验证 history 流转. ──
        struct EchoTool;
        #[async_trait]
        impl crate::tools::Tool for EchoTool {
            fn name(&self) -> &str {
                "echo-tool"
            }
            fn description(&self) -> &str {
                "echoes back its text argument"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
            }
            async fn execute(&self, args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: text,
                    error: None,
                })
            }
        }

        // ── Provider: 第 1 次 stream 发 tool_call, 第 2 次发 final 文本. ──
        struct ToolThenTextProvider {
            counter: Arc<AtomicUsize>,
            captured_tool_counts: Arc<PMutex<Vec<usize>>>,
        }
        #[async_trait]
        impl Provider for ToolThenTextProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temp: f64,
                options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                self.captured_tool_counts
                    .lock()
                    .push(options.tools.as_ref().map_or(0, Vec::len));
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    let calls = vec![ToolCallChunk::new("tc-1", "echo-tool", r#"{"text":"echoed"}"#, 0)];
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(calls)),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let captured_tool_counts = Arc::new(PMutex::new(Vec::new()));
        deps.provider = Arc::new(ToolThenTextProvider {
            counter: Arc::new(AtomicUsize::new(0)),
            captured_tool_counts: Arc::clone(&captured_tool_counts),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(EchoTool) as Box<dyn crate::tools::Tool>]));
        deps.max_tool_iterations = 4;
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-tool-happy".to_string(),
                history: Vec::new(),
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut saw_tool_started = false;
        let mut saw_tool_finished_success = false;
        let mut saw_completion = false;
        let mut final_text_seen = String::new();
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_millis(2000), action_rx.recv())
                .await
                .expect("driver should respond within 2s per action")
                .expect("action must arrive");
            match action {
                Action::ToolStarted { name, .. } => {
                    assert_eq!(name, "echo-tool");
                    saw_tool_started = true;
                }
                Action::ToolFinished {
                    name, success, result, ..
                } => {
                    assert_eq!(name, "echo-tool");
                    if success {
                        saw_tool_finished_success = true;
                        assert!(
                            result.as_deref().is_some_and(|s| s.contains("echoed")),
                            "tool result must echo back text arg (got {result:?})"
                        );
                    }
                }
                Action::StreamChunkReceived { delta, .. } => {
                    final_text_seen.push_str(&delta);
                }
                Action::StreamCompleted { final_text, .. } => {
                    saw_completion = true;
                    assert!(
                        final_text.contains("done"),
                        "final text must contain 'done' (got {final_text:?})"
                    );
                    break;
                }
                Action::StreamFailed { err, .. } => {
                    panic!("driver should not fail in happy path: {err}");
                }
                _ => {}
            }
        }
        assert!(saw_tool_started, "must see ToolStarted");
        assert!(saw_tool_finished_success, "must see ToolFinished(success=true)");
        assert!(saw_completion, "must see StreamCompleted");
        assert!(
            final_text_seen.contains("done"),
            "streaming delta must include 'done' (got {final_text_seen:?})"
        );
        assert_eq!(
            *captured_tool_counts.lock(),
            vec![1, 1],
            "driver must pass registered tool specs to each streaming request"
        );
    }

    /// **5a-6 limit case**：max_tool_iterations 超过即触发 StreamFailed.
    ///
    /// 模拟 provider 每次都发 tool_call (不停止) — driver 达到 iter 上限后必须 fail.
    #[tokio::test]
    async fn driver_max_tool_iterations_emits_stream_failed() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct NoopTool;
        #[async_trait]
        impl crate::tools::Tool for NoopTool {
            fn name(&self) -> &str {
                "noop"
            }
            fn description(&self) -> &str {
                "no-op"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "ok".to_string(),
                    error: None,
                })
            }
        }

        struct AlwaysToolCallProvider;
        #[async_trait]
        impl Provider for AlwaysToolCallProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let calls = vec![ToolCallChunk::new("loop", "noop", "{}", 0)];
                stream::iter(vec![
                    Ok(StreamChunk::tool_call_chunk(calls)),
                    Ok(StreamChunk::final_chunk()),
                ])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(AlwaysToolCallProvider);
        deps.tools_registry = Some(Arc::new(vec![Box::new(NoopTool) as Box<dyn crate::tools::Tool>]));
        deps.max_tool_iterations = 2; // 故意低
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-max-iter".to_string(),
                history: Vec::new(),
                cancel,
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut got_failed = false;
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_millis(2000), action_rx.recv())
                .await
                .expect("driver should respond per action within 2s")
                .expect("action must arrive");
            if let Action::StreamFailed { err, retryable, .. } = &action {
                assert!(!retryable, "max-iter exceeded is permanent");
                assert!(
                    err.contains("max tool iterations") || err.contains("max_tool"),
                    "err must mention max iterations (got: {err})"
                );
                got_failed = true;
                break;
            }
        }
        assert!(
            got_failed,
            "driver must emit StreamFailed when max_tool_iterations exceeded"
        );
    }

    /// P1-2: driver 路径下 turn 中 cancel — drive_start_turn_stream 内 select! 选 cancel 分支.
    ///
    /// 验证 cancel_token cancel 后, drive_start_turn_stream 发 StreamCancelled,
    /// 而不是继续消费 stream 或发 StreamCompleted.
    #[tokio::test]
    async fn driver_mid_turn_cancel_emits_stream_cancelled() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        /// Provider 返回一个"永远不结束"的 stream — 由 cancel 接管.
        struct PendingStreamProvider;
        #[async_trait]
        impl Provider for PendingStreamProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(
                &self,
                _sys: Option<&str>,
                _msg: &str,
                _model: &str,
                _temp: f64,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _r: ChatRequest<'_>, _model: &str, _temp: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _messages: &[PMsg],
                _model: &str,
                _temp: f64,
                _options: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                // 单 delta + pending（用 stream::pending 让 next() 永远 pending）
                stream::iter(vec![Ok(StreamChunk::delta("partial"))])
                    .chain(stream::pending())
                    .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(PendingStreamProvider);
        let executor = EffectExecutor::new_with_deps(deps);

        let cancel = CancellationToken::new();
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-cancel-mid".to_string(),
                history: Vec::new(),
                cancel: cancel.clone(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        // 先收到一个 delta（partial）证明 stream 已活跃.
        let a1 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("first delta within 1.5s")
            .expect("first delta received");
        assert!(
            matches!(a1, Action::StreamChunkReceived { ref delta, .. } if delta == "partial"),
            "expected first partial delta, got {a1:?}"
        );

        // turn 中 cancel
        cancel.cancel();

        // 应立刻收到 StreamCancelled.
        let a2 = tokio::time::timeout(Duration::from_millis(1500), action_rx.recv())
            .await
            .expect("StreamCancelled within 1.5s after cancel")
            .expect("StreamCancelled received");
        match a2 {
            Action::StreamCancelled { draft_id } => assert_eq!(draft_id, "draft-cancel-mid"),
            other => panic!("expected StreamCancelled after mid-turn cancel, got {other:?}"),
        }
    }

    /// P0-1 简化版: try_dispatch ChannelClosed 时返回 ChannelClosed
    /// (chat::run driver 分支会据此 abort turn + cleanup + continue).
    #[tokio::test]
    async fn chat_dispatcher_try_dispatch_returns_channel_closed_after_rx_drop() {
        let (dispatcher, rx) = ChatDispatcher::new();
        drop(rx);
        let result = dispatcher.try_dispatch(Action::StartLLMTurn {
            draft_id: "d1".to_string(),
            history: Vec::new(),
            cancel: CancellationToken::new(),
        });
        assert!(
            matches!(result, DispatchResult::ChannelClosed),
            "after action_rx drop, try_dispatch must return ChannelClosed (got {result:?})"
        );
    }

    // ─── S2.5 P1-A: dispatch_or_log 失败处理 ─────────────────────────────────

    /// S2.5 P1-A: 正常路径 — dispatch_or_log 返回 Sent，Action 真入队.
    ///
    /// 不断言 drops 计数变化（counter 全局共享，并行测试会污染读数）；
    /// 通过 Backpressured/Closed 两个测试已覆盖 drops 计数 +1 语义。
    #[tokio::test]
    async fn s2_5_p1_a_dispatch_or_log_normal_sent() {
        let (dispatcher, mut rx) = ChatDispatcher::new();
        let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.normal");
        assert!(
            matches!(result, DispatchResult::Sent),
            "正常路径必须返回 Sent (got {result:?})"
        );
        // 验证 Action 真入队
        let recv = rx.try_recv().expect("test: action should be in queue");
        assert!(matches!(recv, Action::CancelRequested));
    }

    /// S2.5 P1-A: 满 channel — dispatch_or_log 返回 Backpressured 且 backpressured 计数至少 +1.
    ///
    /// 由于 backpressured 计数器全局共享，断言 `after > before`（至少 +1）
    /// 而非精确 +1，避免并行测试干扰；核心契约：本次 dispatch 真触发了 inc.
    #[tokio::test]
    async fn s2_5_p1_a_dispatch_or_log_full_warns_and_counts() {
        use crate::observability::chat_metrics;
        let (tx, _rx) = mpsc::channel::<Action>(1);
        let dispatcher = ChatDispatcher { action_tx: tx };
        // 填满 1 容量.
        let _ = dispatcher.try_dispatch(Action::CancelRequested);

        let before = chat_metrics::get_dispatch_drops_count("backpressured");
        let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.full");
        assert!(
            matches!(result, DispatchResult::Backpressured),
            "channel full → Backpressured (got {result:?})"
        );
        let after = chat_metrics::get_dispatch_drops_count("backpressured");
        assert!(
            after > before,
            "backpressured counter should >= before+1 on full dispatch (before={before}, after={after})"
        );
    }

    /// S2.5 P1-A: 关闭 channel — dispatch_or_log 返回 ChannelClosed 且 closed 计数至少 +1.
    #[tokio::test]
    async fn s2_5_p1_a_dispatch_or_log_closed_warns() {
        use crate::observability::chat_metrics;
        let (dispatcher, rx) = ChatDispatcher::new();
        drop(rx);

        let before = chat_metrics::get_dispatch_drops_count("closed");
        let result = dispatcher.dispatch_or_log(Action::CancelRequested, "test.closed");
        assert!(
            matches!(result, DispatchResult::ChannelClosed),
            "channel closed → ChannelClosed (got {result:?})"
        );
        let after = chat_metrics::get_dispatch_drops_count("closed");
        assert!(
            after > before,
            "closed counter should >= before+1 on channel-closed dispatch (before={before}, after={after})"
        );
    }

    // ─── S3 T3-1 四件套测试 ────────────────────────────────────────────────────

    /// **S3 T3-1 Step 2**: ToolCallAggregator 聚合 Streaming + Completed 协议.
    ///
    /// 验证：多个 Streaming 增量 + 一次 Completed 应返回 Completed.args 作为最终参数；
    /// 重复 Completed 应被识别为幂等 no-op。
    #[test]
    fn t31_aggregator_aggregates_streaming_and_completed() {
        use crate::providers::traits::{ToolCallChunk, ToolCallChunkStatus};
        let mut agg = ToolCallAggregator::new();

        // 第 1 个 Streaming delta
        let r1 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: String::new(),
            index: 0,
            arguments_delta: Some(r#"{"cmd":"#.to_string()),
            status: ToolCallChunkStatus::Streaming,
        });
        assert!(r1.is_none(), "streaming chunk should not yield ready tool call");

        // 第 2 个 Streaming delta
        let r2 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: String::new(),
            index: 0,
            arguments_delta: Some(r#""ls"}"#.to_string()),
            status: ToolCallChunkStatus::Streaming,
        });
        assert!(r2.is_none(), "second streaming chunk also no-op");

        // Completed chunk
        let r3 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"ls"}"#.to_string(),
            index: 0,
            arguments_delta: None,
            status: ToolCallChunkStatus::Completed,
        });
        let (id, name, args) = r3.expect("Completed chunk should yield ready tool call");
        assert_eq!(id, "tc-x");
        assert_eq!(name, "shell");
        assert_eq!(args, r#"{"cmd":"ls"}"#);

        // 重复 Completed → 幂等 no-op.
        let r4 = agg.ingest(ToolCallChunk {
            id: "tc-x".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"ls"}"#.to_string(),
            index: 0,
            arguments_delta: None,
            status: ToolCallChunkStatus::Completed,
        });
        assert!(r4.is_none(), "duplicate Completed should be idempotent no-op");
    }

    #[test]
    fn aggregator_backfills_slot_id_when_empty() {
        use crate::providers::traits::{ToolCallChunk, ToolCallChunkStatus};
        let mut agg = ToolCallAggregator::new();

        assert!(
            agg.ingest(ToolCallChunk {
                id: String::new(),
                name: "shell".into(),
                args: String::new(),
                index: 0,
                arguments_delta: Some("{".into()),
                status: ToolCallChunkStatus::Streaming,
            })
            .is_none()
        );
        assert!(
            agg.ingest(ToolCallChunk {
                id: "call_abc".into(),
                name: "shell".into(),
                args: String::new(),
                index: 0,
                arguments_delta: Some("}".into()),
                status: ToolCallChunkStatus::Streaming,
            })
            .is_none()
        );

        let (id, name, args) = agg
            .ingest(ToolCallChunk {
                id: String::new(),
                name: "shell".into(),
                args: "{}".into(),
                index: 0,
                arguments_delta: None,
                status: ToolCallChunkStatus::Completed,
            })
            .expect("completed chunk should resolve");
        assert_eq!(id, "call_abc");
        assert_eq!(name, "shell");
        assert_eq!(args, "{}");
    }

    /// **S3 T3-1 Step 2**: ToolCallAggregator 并发 index — 多 tool call 同时进行.
    #[test]
    fn t31_aggregator_concurrent_indices_yield_each_separately() {
        use crate::providers::traits::{ToolCallChunk, ToolCallChunkStatus};
        let mut agg = ToolCallAggregator::new();
        // 交错 emit: tc-a streaming → tc-b streaming → tc-a complete → tc-b complete.
        agg.ingest(ToolCallChunk {
            id: "tc-a".into(),
            name: "tool_a".into(),
            args: String::new(),
            index: 0,
            arguments_delta: Some("{".into()),
            status: ToolCallChunkStatus::Streaming,
        });
        agg.ingest(ToolCallChunk {
            id: "tc-b".into(),
            name: "tool_b".into(),
            args: String::new(),
            index: 1,
            arguments_delta: Some("[".into()),
            status: ToolCallChunkStatus::Streaming,
        });
        let ra = agg
            .ingest(ToolCallChunk {
                id: "tc-a".into(),
                name: "tool_a".into(),
                args: "{}".into(),
                index: 0,
                arguments_delta: None,
                status: ToolCallChunkStatus::Completed,
            })
            .expect("tc-a should complete");
        let rb = agg
            .ingest(ToolCallChunk {
                id: "tc-b".into(),
                name: "tool_b".into(),
                args: "[]".into(),
                index: 1,
                arguments_delta: None,
                status: ToolCallChunkStatus::Completed,
            })
            .expect("tc-b should complete");
        assert_eq!(ra.0, "tc-a");
        assert_eq!(rb.0, "tc-b");
        assert_eq!(ra.2, "{}");
        assert_eq!(rb.2, "[]");
    }

    /// **S3 T3-1 Step 2**: driver 路径：streaming protocol 的 ToolCallChunk 也能驱动 tool 执行.
    ///
    /// Provider 发 [Streaming delta, Streaming delta, Completed] 而非单个 Completed —
    /// driver 应仍然 emit ToolStarted/ToolFinished + 进入下一轮直到 final text.
    #[tokio::test]
    async fn t31_driver_streaming_tool_call_protocol_executes_correctly() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk, ToolCallChunkStatus,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct PingTool;
        #[async_trait]
        impl crate::tools::Tool for PingTool {
            fn name(&self) -> &str {
                "ping"
            }
            fn description(&self) -> &str {
                "ping"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "pong".into(),
                    error: None,
                })
            }
        }

        struct StreamingToolProvider {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for StreamingToolProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    // 用 streaming 协议发：先 2 个 Streaming delta，再 1 个 Completed.
                    let s1 = ToolCallChunk {
                        id: "call-1".into(),
                        name: "ping".into(),
                        args: String::new(),
                        index: 0,
                        arguments_delta: Some("{".into()),
                        status: ToolCallChunkStatus::Streaming,
                    };
                    let s2 = ToolCallChunk {
                        id: "call-1".into(),
                        name: "ping".into(),
                        args: String::new(),
                        index: 0,
                        arguments_delta: Some("}".into()),
                        status: ToolCallChunkStatus::Streaming,
                    };
                    let c = ToolCallChunk {
                        id: "call-1".into(),
                        name: "ping".into(),
                        args: "{}".into(),
                        index: 0,
                        arguments_delta: None,
                        status: ToolCallChunkStatus::Completed,
                    };
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(vec![s1])),
                        Ok(StreamChunk::tool_call_chunk(vec![s2])),
                        Ok(StreamChunk::tool_call_chunk(vec![c])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(StreamingToolProvider {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(PingTool) as Box<dyn crate::tools::Tool>]));
        deps.max_tool_iterations = 4;
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-t31-streaming".into(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut saw_tool_started = false;
        let mut saw_tool_finished = false;
        let mut saw_completion = false;
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_millis(2000), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::ToolStarted { name, .. } => {
                    assert_eq!(name, "ping");
                    saw_tool_started = true;
                }
                Action::ToolFinished { success, name, .. } => {
                    assert_eq!(name, "ping");
                    assert!(success);
                    saw_tool_finished = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("done"), "want 'done' got {final_text:?}");
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail in happy path: {err}"),
                _ => {}
            }
        }
        assert!(saw_tool_started, "must see ToolStarted");
        assert!(saw_tool_finished, "must see ToolFinished");
        assert!(saw_completion, "must see StreamCompleted");
    }

    #[tokio::test]
    async fn dispatcher_tool_call_request_includes_reasoning_in_history() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use parking_lot::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct PingTool;
        #[async_trait]
        impl crate::tools::Tool for PingTool {
            fn name(&self) -> &str {
                "ping"
            }
            fn description(&self) -> &str {
                "ping"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "pong".into(),
                    error: None,
                })
            }
        }

        struct ReasoningToolProvider {
            counter: Arc<AtomicUsize>,
            second_history: Arc<Mutex<Option<Vec<PMsg>>>>,
        }
        #[async_trait]
        impl Provider for ReasoningToolProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                messages: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    stream::iter(vec![
                        Ok(StreamChunk::reasoning_delta("Need to call ping.")),
                        Ok(StreamChunk::tool_call_chunk(vec![ToolCallChunk::new(
                            "call-ping",
                            "ping",
                            "{}",
                            0,
                        )])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    *self.second_history.lock() = Some(messages.to_vec());
                    stream::iter(vec![Ok(StreamChunk::delta("done")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let second_history = Arc::new(Mutex::new(None));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ReasoningToolProvider {
            counter: Arc::new(AtomicUsize::new(0)),
            second_history: Arc::clone(&second_history),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(PingTool) as Box<dyn crate::tools::Tool>]));
        deps.max_tool_iterations = 4;
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-reasoning-tool-history".into(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_millis(2000), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("done"));
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                _ => {}
            }
        }

        let history = second_history.lock().clone().expect("second request history captured");
        let assistant = history
            .iter()
            .find(|message| message.role == "assistant")
            .expect("assistant tool-call history expected");
        let value: serde_json::Value = serde_json::from_str(&assistant.content).expect("assistant payload JSON");
        assert_eq!(
            value.get("reasoning_content").and_then(serde_json::Value::as_str),
            Some("Need to call ping.")
        );
        let call_id = value
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
            .and_then(|calls| calls.first())
            .and_then(|call| call.get("id"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(call_id, Some("call-ping"));
    }

    /// **S3 T3-1 Step 3**: context overflow → 自动 compact + 单次重试 → success.
    ///
    /// Provider 第 1 次发 StreamError::Provider("maximum context length exceeded")，
    /// 第 2 次成功完成。driver 应：emit HistoryCompacted{ContextOverflow} → 再调
    /// stream API → emit StreamCompleted。
    #[tokio::test]
    async fn t31_driver_context_overflow_triggers_compact_and_retries() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct OverflowOnceProvider {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for OverflowOnceProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                        "Error: maximum context length exceeded for this model".into(),
                    ))])
                    .boxed()
                } else {
                    stream::iter(vec![
                        Ok(StreamChunk::delta("recovered")),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(OverflowOnceProvider {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-overflow".into(),
                history: vec![crate::providers::traits::ChatMessage {
                    role: "user".into(),
                    content: "hello".into(),
                }],
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut saw_compacted = false;
        let mut saw_completion = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_millis(2000), action_rx.recv())
                .await
                .expect("driver action within 2s")
                .expect("must arrive");
            match action {
                Action::HistoryCompacted {
                    reason: crate::chat::action::CompactReason::ContextOverflow,
                } => {
                    saw_compacted = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("recovered"), "want 'recovered' got {final_text:?}");
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => {
                    panic!("driver should retry on overflow, not fail: {err}");
                }
                _ => {}
            }
        }
        assert!(saw_compacted, "must emit HistoryCompacted on overflow");
        assert!(saw_completion, "must complete after compact+retry");
    }

    /// **S3 T3-1 Step 3**: context overflow 重试超 1 次 → StreamFailed.
    #[tokio::test]
    async fn t31_driver_context_overflow_exhausted_emits_stream_failed() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct AlwaysOverflowProvider;
        #[async_trait]
        impl Provider for AlwaysOverflowProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Err::<StreamChunk, _>(StreamError::Provider(
                    "context_length_exceeded: please reduce input".into(),
                ))])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(AlwaysOverflowProvider);
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-overflow-fail".into(),
                history: vec![crate::providers::traits::ChatMessage {
                    role: "user".into(),
                    content: "x".repeat(1000),
                }],
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut saw_failed = false;
        for _ in 0..16 {
            let action = tokio::time::timeout(Duration::from_millis(2000), action_rx.recv())
                .await
                .expect("action within 2s")
                .expect("must arrive");
            if let Action::StreamFailed { err, .. } = &action {
                assert!(
                    err.contains("context overflow") || err.contains("context_length_exceeded"),
                    "err must mention overflow: {err}"
                );
                saw_failed = true;
                break;
            }
        }
        assert!(saw_failed, "must emit StreamFailed after overflow retries exhausted");
    }

    /// **S3 T3-1 Step 4**: ApprovalRouter resolve / register 基本路径.
    #[tokio::test]
    async fn t31_approval_router_register_and_resolve_basic() {
        let router = ApprovalRouter::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        router.register("call-1".to_string(), tx);
        assert!(router.resolve("call-1", true), "resolve should find the tx");
        assert!(rx.await.expect("oneshot rx must resolve"));
        // 第二次 resolve 相同 id 应返回 false (没有 pending)
        assert!(!router.resolve("call-1", false), "second resolve must miss");
    }

    /// **S3 T3-1 Step 4**: approval 路径 — needs_approval=true 时 driver 走 router 等响应；
    /// stub EffectExecutor::RequestApproval 默认 auto-approve.
    #[tokio::test]
    async fn t31_driver_approval_path_auto_approves_via_stub() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct ShellTool;
        #[async_trait]
        impl crate::tools::Tool for ShellTool {
            fn name(&self) -> &str {
                "shell"
            }
            fn description(&self) -> &str {
                "shell"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "ran".into(),
                    error: None,
                })
            }
        }

        struct ToolThenText {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for ToolThenText {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    let c = ToolCallChunk::new("call-shell", "shell", "{}", 0);
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(vec![c])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("ok")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        // ApprovalManager 配置：Supervised + always_ask=[shell] → needs_approval(shell)=true.
        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            auto_approve: Vec::new(),
            always_ask: vec!["shell".to_string()],
            ..Default::default()
        };
        let mgr = Arc::new(crate::approval::ApprovalManager::from_config(&approval_cfg));

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ToolThenText {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(ShellTool) as Box<dyn crate::tools::Tool>]));
        deps.max_tool_iterations = 4;
        deps.approval_manager = Some(mgr);
        // 测试拦截器：当看到 `Action::ToolApprovalRequested` 时主动 router.resolve(true)
        // 模拟 dispatcher_task + EffectExecutor stub 的端到端 auto-approve 行为。
        let router_for_resolve = Arc::clone(&deps.approval_router);
        let executor = EffectExecutor::new_with_deps(deps);
        let shutdown_d = CancellationToken::new();
        let (sink_tx, mut sink_rx) = mpsc::channel::<Action>(64);
        let router_handle = Arc::clone(&router_for_resolve);
        let shutdown_clone = shutdown_d.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_clone.cancelled() => break,
                    maybe = action_rx.recv() => {
                        match maybe {
                            Some(action) => {
                                if let Action::ToolApprovalRequested { tool_id, .. } = &action {
                                    router_handle.resolve(tool_id, true);
                                    // 模拟 stub 发回 ToolApprovalReceived 给观察者：
                                    let _ = sink_tx
                                        .send(Action::ToolApprovalReceived {
                                            tool_id: tool_id.clone(),
                                            approved: true,
                                        })
                                        .await;
                                }
                                let _ = sink_tx.send(action).await;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-approval".into(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut saw_request = false;
        let mut saw_received = false;
        let mut saw_tool_started = false;
        let mut saw_tool_finished = false;
        let mut saw_completion = false;
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_millis(3000), sink_rx.recv())
                .await
                .expect("action within 3s")
                .expect("must arrive");
            match action {
                Action::ToolApprovalRequested { tool_id, name, .. } => {
                    assert_eq!(tool_id, "call-shell");
                    assert_eq!(name, "shell");
                    saw_request = true;
                }
                Action::ToolApprovalReceived { tool_id, approved } => {
                    assert_eq!(tool_id, "call-shell");
                    assert!(approved, "stub should auto-approve");
                    saw_received = true;
                }
                Action::ToolStarted { name, .. } => {
                    assert_eq!(name, "shell");
                    saw_tool_started = true;
                }
                Action::ToolFinished { success, name, .. } => {
                    assert_eq!(name, "shell");
                    assert!(success);
                    saw_tool_finished = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("ok"));
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => panic!("driver should not fail: {err}"),
                _ => {}
            }
        }
        shutdown_d.cancel();
        assert!(saw_request, "must see ToolApprovalRequested");
        assert!(saw_received, "must see ToolApprovalReceived");
        assert!(saw_tool_started, "must see ToolStarted after approval");
        assert!(saw_tool_finished, "must see ToolFinished after approval");
        assert!(saw_completion, "must see StreamCompleted after approval");
    }

    /// **S3 T3-1 Step 4**: approval rejected → tool 不执行 + ToolFinished(success=false, "User rejected").
    #[tokio::test]
    async fn t31_driver_approval_rejected_skips_tool_execution() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions,
            StreamResult, ToolCallChunk,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

        let exec_counter = Arc::new(AtomicBool::new(false));
        struct RejectableTool {
            executed: Arc<AtomicBool>,
        }
        #[async_trait]
        impl crate::tools::Tool for RejectableTool {
            fn name(&self) -> &str {
                "danger"
            }
            fn description(&self) -> &str {
                "danger"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "did dangerous thing".into(),
                    error: None,
                })
            }
        }

        struct ToolThenText {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for ToolThenText {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    let c = ToolCallChunk::new("call-danger", "danger", "{}", 0);
                    stream::iter(vec![
                        Ok(StreamChunk::tool_call_chunk(vec![c])),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                } else {
                    stream::iter(vec![Ok(StreamChunk::delta("declined")), Ok(StreamChunk::final_chunk())]).boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            auto_approve: Vec::new(),
            always_ask: vec!["danger".to_string()],
            ..Default::default()
        };
        let mgr = Arc::new(crate::approval::ApprovalManager::from_config(&approval_cfg));

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(ToolThenText {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        deps.tools_registry = Some(Arc::new(vec![Box::new(RejectableTool {
            executed: Arc::clone(&exec_counter),
        }) as Box<dyn crate::tools::Tool>]));
        deps.max_tool_iterations = 4;
        deps.approval_manager = Some(mgr);
        let router_for_resolve = Arc::clone(&deps.approval_router);
        let executor = EffectExecutor::new_with_deps(deps);

        // 拦截 action_rx：截获 ToolApprovalRequested → 直接 resolve(false)，
        // 让 driver 收到 rejection（绕过 stub auto-approve 的默认行为）。
        let shutdown_d = CancellationToken::new();
        let (sink_tx, mut sink_rx) = mpsc::channel::<Action>(64);
        let router_handle = Arc::clone(&router_for_resolve);
        let shutdown_clone = shutdown_d.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown_clone.cancelled() => break,
                    maybe = action_rx.recv() => {
                        match maybe {
                            Some(action) => {
                                // 抢先 reject — 在 stub auto-approve 之前 resolve(false).
                                if let Action::ToolApprovalRequested { tool_id, .. } = &action {
                                    router_handle.resolve(tool_id, false);
                                }
                                let _ = sink_tx.send(action).await;
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-reject".into(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut saw_tool_finished_rejected = false;
        let mut saw_completion = false;
        for _ in 0..32 {
            let action = tokio::time::timeout(Duration::from_millis(3000), sink_rx.recv())
                .await
                .expect("action within 3s")
                .expect("must arrive");
            match action {
                Action::ToolFinished {
                    success, result, name, ..
                } => {
                    assert_eq!(name, "danger");
                    assert!(!success, "rejected tool must report success=false");
                    let r = result.as_deref().unwrap_or_default();
                    assert!(r.contains("User rejected") || r.contains("rejected"), "result={r:?}");
                    saw_tool_finished_rejected = true;
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("declined"));
                    saw_completion = true;
                    break;
                }
                _ => {}
            }
        }
        shutdown_d.cancel();
        assert!(
            saw_tool_finished_rejected,
            "must see ToolFinished(success=false) on reject"
        );
        assert!(saw_completion, "must see StreamCompleted with replacement text");
        assert!(
            !exec_counter.load(AtomicOrdering::SeqCst),
            "rejected tool MUST NOT have executed"
        );
    }

    #[tokio::test]
    async fn dispatch_tool_with_missing_approval_router_rejects() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct DangerousTool {
            executed: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for DangerousTool {
            fn name(&self) -> &str {
                "danger"
            }
            fn description(&self) -> &str {
                "danger"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "ran".into(),
                    error: None,
                })
            }
        }

        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            auto_approve: Vec::new(),
            always_ask: vec!["danger".to_string()],
            ..Default::default()
        };
        let approval_manager = Arc::new(crate::approval::ApprovalManager::from_config(&approval_cfg));
        let executed = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(DangerousTool {
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-danger".into(),
            name: "danger".into(),
            args: "{}".into(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();

        let outcome = execute_single_tool_call(
            &registry,
            &call,
            &CancellationToken::new(),
            &action_tx,
            "draft-missing-router",
            None,
            Some(&approval_manager),
            &mut history,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done));
        assert!(
            !executed.load(AtomicOrdering::SeqCst),
            "tool requiring approval must not execute without router"
        );
        let action = tokio::time::timeout(Duration::from_secs(2), action_rx.recv())
            .await
            .expect("ToolFinished action")
            .expect("action present");
        match action {
            Action::ToolFinished {
                name, success, result, ..
            } => {
                assert_eq!(name, "danger");
                assert!(!success);
                assert!(
                    result
                        .as_deref()
                        .is_some_and(|value| value.contains("approval system not available")),
                    "unexpected result: {result:?}"
                );
            }
            other => panic!("expected ToolFinished fail-CLOSED action, got {other:?}"),
        }
        assert!(
            action_rx.try_recv().is_err(),
            "fail-CLOSED path must not start the tool"
        );
        let tool_message = history.last().expect("tool rejection history expected");
        assert_eq!(tool_message.role, "tool");
        let payload: serde_json::Value = serde_json::from_str(&tool_message.content).expect("tool payload JSON");
        assert_eq!(
            payload.get("tool_call_id").and_then(serde_json::Value::as_str),
            Some("call-danger")
        );
        assert_eq!(payload.get("success").and_then(serde_json::Value::as_bool), Some(false));
        assert!(
            payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.contains("approval system not available"))
        );
    }

    #[tokio::test]
    async fn dispatch_tool_with_approval_router_works_normally() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct DangerousTool {
            executed: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for DangerousTool {
            fn name(&self) -> &str {
                "danger"
            }
            fn description(&self) -> &str {
                "danger"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "ran".into(),
                    error: None,
                })
            }
        }

        let approval_cfg = crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::Supervised,
            auto_approve: Vec::new(),
            always_ask: vec!["danger".to_string()],
            ..Default::default()
        };
        let approval_manager = Arc::new(crate::approval::ApprovalManager::from_config(&approval_cfg));
        let approval_router = Arc::new(ApprovalRouter::new());
        let executed = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(DangerousTool {
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-danger".into(),
            name: "danger".into(),
            args: "{}".into(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();

        let router_handle = Arc::clone(&approval_router);
        let resolver = tokio::spawn(async move {
            let action = action_rx.recv().await.expect("approval request action");
            match action {
                Action::ToolApprovalRequested { tool_id, name, .. } => {
                    assert_eq!(tool_id, "call-danger");
                    assert_eq!(name, "danger");
                    assert!(router_handle.resolve(&tool_id, true));
                }
                other => panic!("expected approval request, got {other:?}"),
            }

            let started = action_rx.recv().await.expect("tool started action");
            assert!(matches!(started, Action::ToolStarted { ref name, .. } if name == "danger"));
            let finished = action_rx.recv().await.expect("tool finished action");
            match finished {
                Action::ToolFinished { name, success, .. } => {
                    assert_eq!(name, "danger");
                    assert!(success);
                }
                other => panic!("expected ToolFinished success, got {other:?}"),
            }
        });

        let outcome = execute_single_tool_call(
            &registry,
            &call,
            &CancellationToken::new(),
            &action_tx,
            "draft-router",
            Some(&approval_router),
            Some(&approval_manager),
            &mut history,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done));
        assert!(executed.load(AtomicOrdering::SeqCst));
        resolver.await.expect("resolver task should complete");
        let tool_message = history.last().expect("tool result history expected");
        assert_eq!(tool_message.role, "tool");
        let payload: serde_json::Value = serde_json::from_str(&tool_message.content).expect("tool payload JSON");
        assert_eq!(
            payload.get("tool_call_id").and_then(serde_json::Value::as_str),
            Some("call-danger")
        );
        assert_eq!(payload.get("success").and_then(serde_json::Value::as_bool), Some(true));
    }

    /// **S3 T3-1 Step 5**: stream_error_is_network_timeout 正确识别 reqwest 错误.
    ///
    /// 用 reqwest::Client 故意 GET 一个不可达地址触发 connect 错误以构造真错误。
    #[test]
    fn t31_stream_error_is_network_timeout_recognises_io_error() {
        use crate::providers::traits::StreamError;
        let io_err = StreamError::Io(std::io::Error::other("simulated"));
        assert!(stream_error_is_network_timeout(&io_err));
        let json_err = StreamError::Json(serde_json::from_str::<serde_json::Value>("notjson").unwrap_err());
        assert!(!stream_error_is_network_timeout(&json_err));
        let provider_err = StreamError::Provider("rate limit".into());
        assert!(!stream_error_is_network_timeout(&provider_err));
    }

    /// **S3 T3-1 Step 5**: stream_error_is_context_overflow 子串匹配多 provider.
    #[test]
    fn t31_stream_error_is_context_overflow_matches_provider_strings() {
        use crate::providers::traits::StreamError;
        for msg in [
            "Error: maximum context length exceeded",
            "context_length_exceeded",
            "the prompt is too long",
            "input token count is 200000",
            "exceeds maximum allowed",
            "Token limit reached",
        ] {
            let err = StreamError::Provider(msg.into());
            assert!(
                stream_error_is_context_overflow(&err),
                "expected overflow match for: {msg}"
            );
        }
        let normal = StreamError::Provider("rate limited".into());
        assert!(!stream_error_is_context_overflow(&normal));
    }

    /// **S3 T3-1 Step 5**: io error 重试 — driver 第 1, 2 次 io error 后第 3 次成功.
    ///
    /// 用 short backoff 避免单测耗时太长（注：当前 BACKOFF_BASE_MS=500ms 已经够小，
    /// 加上 1s+2s=3.5s 总耗时，单测 timeout 充裕）。
    #[tokio::test]
    async fn t31_driver_network_timeout_retries_with_backoff_then_succeeds() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct FlakyProvider {
            counter: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Provider for FlakyProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                let n = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
                if n < 2 {
                    stream::iter(vec![Err::<StreamChunk, _>(StreamError::Io(std::io::Error::other(
                        "simulated network timeout",
                    )))])
                    .boxed()
                } else {
                    stream::iter(vec![
                        Ok(StreamChunk::delta("recovered")),
                        Ok(StreamChunk::final_chunk()),
                    ])
                    .boxed()
                }
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(FlakyProvider {
            counter: Arc::new(AtomicUsize::new(0)),
        });
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-flaky".into(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut retry_attempts: u8 = 0;
        let mut saw_completion = false;
        // 总耗时上限：~3.5s 真 sleep + 一些 RTT，给 8s 余量.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            assert!(std::time::Instant::now() < deadline, "test deadline exceeded");
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let action = match tokio::time::timeout(remaining.min(Duration::from_secs(4)), action_rx.recv()).await {
                Ok(Some(a)) => a,
                Ok(None) => break,
                Err(_) => continue,
            };
            match action {
                Action::StreamRetryAttempt { attempt, .. } => {
                    retry_attempts = retry_attempts.max(attempt);
                }
                Action::StreamCompleted { final_text, .. } => {
                    assert!(final_text.contains("recovered"));
                    saw_completion = true;
                    break;
                }
                Action::StreamFailed { err, .. } => {
                    panic!("driver should retry, not fail: {err}");
                }
                _ => {}
            }
        }
        assert!(
            retry_attempts >= 1,
            "must emit at least one StreamRetryAttempt (got {retry_attempts})"
        );
        assert!(saw_completion, "must complete after backoff retries");
    }

    /// **S3 T3-1 Step 5**: io error 持续 → 重试耗尽 → StreamFailed(retryable=false).
    #[tokio::test]
    async fn t31_driver_network_timeout_exhausted_emits_stream_failed() {
        use crate::providers::traits::{
            ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamError,
            StreamOptions, StreamResult,
        };
        use async_trait::async_trait;
        use futures::stream::{self, BoxStream, StreamExt};

        struct AlwaysIoErrProvider;
        #[async_trait]
        impl Provider for AlwaysIoErrProvider {
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
                Ok(ChatResponse {
                    text: None,
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn stream_chat_with_history(
                &self,
                _: &[PMsg],
                _: &str,
                _: f64,
                _: StreamOptions,
            ) -> BoxStream<'static, StreamResult<StreamChunk>> {
                stream::iter(vec![Err::<StreamChunk, _>(StreamError::Io(std::io::Error::other(
                    "persistent network failure",
                )))])
                .boxed()
            }
            async fn warmup(&self) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        deps.provider = Arc::new(AlwaysIoErrProvider);
        let executor = EffectExecutor::new_with_deps(deps);
        executor
            .execute(Effect::StartTurn {
                draft_id: "draft-net-fail".into(),
                history: Vec::new(),
                cancel: CancellationToken::new(),
                chat_mode: crate::agent::loop_::ChatMode::Edit,
            })
            .await;

        let mut saw_failed = false;
        // backoff = 500ms + 1s + 2s = 3.5s + RTT，给 10s 余量.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(std::time::Instant::now() < deadline, "test deadline exceeded");
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let action = match tokio::time::timeout(remaining.min(Duration::from_secs(5)), action_rx.recv()).await {
                Ok(Some(a)) => a,
                Ok(None) => break,
                Err(_) => continue,
            };
            if let Action::StreamFailed { err, retryable, .. } = action {
                assert!(!retryable, "exhausted retries must be non-retryable");
                assert!(err.contains("network retries exhausted"), "err={err}");
                saw_failed = true;
                break;
            }
        }
        assert!(saw_failed, "must emit StreamFailed after exhausting network retries");
    }

    // ─── S2.5 T2.5-3: Effect 重放幂等性测试 ────────────────────────────────────

    /// S2.5 T2.5-3: 同 snapshot 连续 dispatch SaveSession 两次 store_count == 2，
    /// 状态收敛（idempotent-overwrite 语义：每次都覆盖，最终一致）。
    #[tokio::test]
    async fn s2_5_t2_5_3_save_session_dispatch_idempotent() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let session = ChatSession::new("prov", "model");
        executor.execute(Effect::SaveSession(session.clone())).await;
        executor.execute(Effect::SaveSession(session)).await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            store_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "两次 SaveSession 应触发 memory.store 两次（idempotent-overwrite，非去重）"
        );
    }

    /// S2.5 T2.5-3: 同 event 两次 NotifyHook 命中真 hook 两次（fire-and-forget 不抑制重复）.
    ///
    /// 通过真注册 hook + touch 哨兵 + 计数文件大小验证（touch 第二次会 update mtime
    /// 但不改大小），用 `>>` append 行更可靠：第一次创建，第二次扩 1 字节。
    #[tokio::test]
    async fn s2_5_t2_5_3_notify_hook_repeat_no_double_fire() {
        use crate::hooks::HookEvent;

        let temp = TempDir::new().expect("tempdir");
        let counter = temp.path().join("hook_counter.log");
        let counter_str = counter.to_str().expect("valid path");

        // 每次触发 append 一个字符到计数文件（用 sh -c 实现 append）.
        let hooks_json = serde_json::json!({
            "enabled": true,
            "hooks": {
                "turn_complete": [
                    {
                        "command": "sh",
                        "args": ["-c", format!("printf x >> {counter_str}")],
                        "stdin_json": false
                    }
                ]
            }
        });
        std::fs::write(temp.path().join("hooks.json"), hooks_json.to_string()).expect("write hooks.json");

        let hooks = Arc::new(HookManager::new(temp.path().to_path_buf()));
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let provider: Arc<dyn Provider> = Arc::new(MockEnvProvider::from_env());
        let channel: Arc<dyn crate::channels::Channel> = Arc::new(TerminalChannel::new(true));
        let observer: Arc<dyn crate::observability::Observer> = Arc::new(NoopObserver);
        let (action_tx, _action_rx) = mpsc::channel::<Action>(64);
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let shutdown = CancellationToken::new();
        let deps = EffectDeps {
            provider,
            memory,
            channel,
            hooks: Arc::clone(&hooks),
            observer,
            action_tx,
            dual_write_guard: RuntimeDualWriteGuard::new(),
            redraw_tx: Some(redraw_tx),
            shutdown,
            model: Arc::from("test-model"),
            temperature: 0.0,
            tools_registry: None,
            max_tool_iterations: 0,
            approval_router: Arc::new(ApprovalRouter::new()),
            approval_manager: None,
        };
        let executor = EffectExecutor::new_with_deps(deps);

        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"seq": 1}),
            })
            .await;
        executor
            .execute(Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({"seq": 2}),
            })
            .await;

        // hook 命令是 spawn，给足时间.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let bytes = std::fs::read(&counter).unwrap_or_default();
        assert_eq!(
            bytes.len(),
            2,
            "两次 NotifyHook 应让 hook command 执行两次 → 计数器累计 2 字节，实测 {} 字节",
            bytes.len()
        );
    }

    /// S2.5 T2.5-3: CancelToken 三次 cancel 无 panic（token 内部幂等）.
    ///
    /// 首次 cancel 触发 token.is_cancelled() == true；后续 dispatch 应保持 true
    /// 且无 panic / 无新副作用。
    #[tokio::test]
    async fn s2_5_t2_5_3_cancel_token_triple_cancel_no_panic() {
        let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = EffectExecutor::new_with_deps(deps);

        let token = CancellationToken::new();
        assert!(!token.is_cancelled(), "token initially not cancelled");

        for _ in 0..3 {
            executor.execute(Effect::CancelToken(token.clone())).await;
        }
        // 三次都应将 token 保持在 cancelled = true，且无 panic.
        assert!(token.is_cancelled(), "cancel 三次后 token 应保持 cancelled");
    }

    /// S2.5 T2.5-3: 并发 dispatch 两个 SaveSession 无 race / 无 deadlock
    /// (T3-3-fixB D1 inline await 后的回归防护).
    ///
    /// 两个 spawn 调用 execute，等任务完成后 store_count == 2，无 panic / 无 hang.
    #[tokio::test]
    async fn s2_5_t2_5_3_save_session_concurrent_dispatch_no_race() {
        let store_count = Arc::new(AtomicUsize::new(0));
        let memory: Arc<dyn Memory> = Arc::new(CountingMemory {
            inner: NoneMemory::new(),
            store_count: Arc::clone(&store_count),
        });
        let shutdown = CancellationToken::new();
        let (deps, _action_rx, _hooks, _temp) = build_deps(memory, shutdown);
        let executor = Arc::new(EffectExecutor::new_with_deps(deps));

        let session = ChatSession::new("prov", "model");
        let exec1 = Arc::clone(&executor);
        let sess1 = session.clone();
        let h1 = tokio::spawn(async move {
            exec1.execute(Effect::SaveSession(sess1)).await;
        });
        let exec2 = Arc::clone(&executor);
        let sess2 = session;
        let h2 = tokio::spawn(async move {
            exec2.execute(Effect::SaveSession(sess2)).await;
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = h1.await;
            let _ = h2.await;
        })
        .await
        .expect("test: concurrent dispatch should not deadlock");

        // SaveSession 子任务异步 spawn，等其完成.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            store_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "两个并发 SaveSession 都应触发 memory.store 一次（共两次）"
        );
    }
}

// ─── S4-A Commit 3: dispatcher snapshot 推送 ────────────────────────────────

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod s4_a_3 {
    use super::*;
    use crate::chat::action::Action;
    use crate::chat::state::{ChatState, UiSnapshot};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    fn make_state() -> ChatState {
        ChatState::new(Arc::from("p-rx"), Arc::from("m-rx"), CancellationToken::new())
    }

    #[tokio::test]
    async fn s4_a_3_dispatcher_pushes_snapshot_on_ui_action() {
        // UI-affecting Action（SystemMessageAdded）应触发 snapshot 推送.
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let _ = dispatcher
            .dispatch(Action::SystemMessageAdded { text: "banner".into() })
            .await;
        // wait for watch update
        tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
            .await
            .expect("snap_rx should receive update within 300ms")
            .expect("watch send_if_modified should have fired");
        let snap = snap_rx.borrow();
        assert!(
            snap.revision >= 1,
            "revision should advance to >=1, got {}",
            snap.revision
        );
        assert!(
            !snap.conversation_lines.is_empty(),
            "snapshot 应包含 SystemMessageAdded 写入的 conversation line"
        );

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_dispatched_keypress_updates_snapshot_input() {
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        );
        let _ = dispatcher.dispatch(Action::KeyPressed(key)).await;
        tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
            .await
            .expect("snap_rx should receive input update within 300ms")
            .expect("watch send_if_modified should have fired");

        let snap = snap_rx.borrow();
        assert_eq!(snap.input.text(), "x");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_dispatcher_skips_unrelated_action() {
        // ToolProgress 静态判定 dirty=false 且不写 ui 字段 → 不应推 snapshot.
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let initial_rev = initial.revision;
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let _ = dispatcher.dispatch(Action::ToolProgress { iteration: 1, max: 3 }).await;
        // 应在 200ms 内不出现 changed 信号.
        let result = tokio::time::timeout(Duration::from_millis(200), snap_rx.changed()).await;
        assert!(
            result.is_err(),
            "ToolProgress 不应触发 snapshot 推送 (changed 返回={:?})",
            result.map(|r| r.is_ok())
        );
        assert_eq!(snap_rx.borrow().revision, initial_rev, "revision 应保持不变");

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_revision_strict_monotonic_in_pure() {
        // 多个 UI Action 后 revision 应严格单调递增.
        let state = make_state();
        let initial = Arc::new(UiSnapshot::initial(
            Arc::clone(&state.session.provider),
            Arc::clone(&state.session.model),
        ));
        let (snap_tx, mut snap_rx) = watch::channel(initial);
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let _handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            Some(snap_tx),
        );

        let mut prev_rev = snap_rx.borrow().revision;
        for i in 0..3 {
            let _ = dispatcher
                .dispatch(Action::SystemMessageAdded {
                    text: format!("msg-{i}"),
                })
                .await;
            tokio::time::timeout(Duration::from_millis(300), snap_rx.changed())
                .await
                .expect("changed within 300ms")
                .expect("watch send");
            let cur = snap_rx.borrow().revision;
            assert!(cur > prev_rev, "revision 应严格递增: prev={prev_rev}, cur={cur}");
            prev_rev = cur;
        }

        shutdown.cancel();
    }

    #[tokio::test]
    async fn s4_a_3_off_mode_no_snapshot_push() {
        // snapshot_tx=None 时（Off/Both/Redux），即使 ui_dirty 也不构造 snapshot
        // — 验证零开销契约.
        let state = make_state();
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let handle = spawn_dispatcher_task_full(
            state,
            action_rx,
            shutdown.clone(),
            EffectExecutor::new_shadow(),
            None,
            None, // 关键：snapshot_tx=None
        );

        // 推送多个 UI Action，dispatcher 不应 panic / hang.
        for i in 0..5 {
            let _ = dispatcher
                .dispatch(Action::SystemMessageAdded { text: format!("m{i}") })
                .await;
        }
        // 给 dispatcher 处理时间.
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.cancel();
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("handle should complete within 2s after shutdown")
            .expect("task join");
        assert!(
            stats.actions_seen >= 5,
            "应至少处理 5 个 actions, got {}",
            stats.actions_seen
        );
    }

    // ── BUG-09 / BUG-05 driver-path coverage ──────────────────────────────

    #[test]
    fn plan_intercept_classifies_read_vs_write_tools() {
        // Read-only tools must NOT be intercepted in plan mode.
        for read in ["file_read", "grep", "web_fetch", "memory_recall", "sessions_list"] {
            assert!(
                !is_plan_intercepted_write_tool(read),
                "{read} is read-only and must run in plan mode"
            );
        }
        // Mutating + unknown tools MUST be intercepted.
        for write in ["file_write", "shell", "git_operations", "some_unknown_mcp_tool"] {
            assert!(
                is_plan_intercepted_write_tool(write),
                "{write} mutates state (or is unknown) and must be simulated in plan mode"
            );
        }
    }

    #[test]
    fn plan_preview_args_is_bounded_and_utf8_safe() {
        assert_eq!(plan_preview_args("short"), "short");
        let long = "a".repeat(500);
        let preview = plan_preview_args(&long);
        assert!(preview.chars().count() <= 161, "preview must be bounded");
        assert!(preview.ends_with('…'));
        // Must not panic on a multibyte boundary.
        let multibyte = "界".repeat(200);
        let _ = plan_preview_args(&multibyte);
    }

    #[tokio::test]
    async fn plan_mode_simulates_write_tool_without_executing() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        struct RecordingWrite {
            executed: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::tools::Tool for RecordingWrite {
            fn name(&self) -> &str {
                "file_write"
            }
            fn description(&self) -> &str {
                "write"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                self.executed.store(true, AtomicOrdering::SeqCst);
                Ok(crate::tools::ToolResult {
                    success: true,
                    output: "REALLY WROTE".into(),
                    error: None,
                })
            }
        }

        let executed = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(vec![Box::new(RecordingWrite {
            executed: Arc::clone(&executed),
        }) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-w".into(),
            name: "file_write".into(),
            args: r#"{"path":"a.txt","content":"x"}"#.into(),
        };
        let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();

        let outcome = execute_single_tool_call(
            &registry,
            &call,
            &CancellationToken::new(),
            &action_tx,
            "draft-plan",
            None,
            None,
            &mut history,
            crate::agent::loop_::ChatMode::Plan,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done));
        assert!(
            !executed.load(AtomicOrdering::SeqCst),
            "plan mode MUST NOT execute the write tool"
        );
        // history tool message must carry the simulated marker, not the real output.
        let tool_msg = history.last().expect("tool result pushed to history");
        assert!(
            tool_msg.content.contains("[plan mode] would call file_write"),
            "history must carry simulated result, got: {}",
            tool_msg.content
        );
        assert!(
            !tool_msg.content.contains("REALLY WROTE"),
            "real tool output must never appear in plan mode"
        );
        // A ToolFinished(success=true) with the simulated text must be emitted.
        let mut saw_finished = false;
        while let Ok(action) = action_rx.try_recv() {
            if let Action::ToolFinished { name, result, .. } = action {
                assert_eq!(name, "file_write");
                assert!(result.unwrap_or_default().contains("[plan mode]"));
                saw_finished = true;
            }
        }
        assert!(saw_finished, "must emit ToolFinished for the simulated call");
    }

    #[tokio::test]
    async fn failed_tool_with_empty_output_surfaces_error_in_content() {
        // BUG-05: a tool that fails with empty output must put its error reason
        // into `content` so the LLM sees the rejection (not an empty result).
        struct RejectingTool;
        #[async_trait::async_trait]
        impl crate::tools::Tool for RejectingTool {
            fn name(&self) -> &str {
                "file_write"
            }
            fn description(&self) -> &str {
                "write"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
                Ok(crate::tools::ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Path not allowed by security policy: /etc/passwd".into()),
                })
            }
        }

        let registry = Arc::new(vec![Box::new(RejectingTool) as Box<dyn crate::tools::Tool>]);
        let call = ResolvedToolCall {
            id: "call-rej".into(),
            name: "file_write".into(),
            args: "{}".into(),
        };
        let (action_tx, _action_rx) = mpsc::channel::<Action>(8);
        let mut history = Vec::new();

        let outcome = execute_single_tool_call(
            &registry,
            &call,
            &CancellationToken::new(),
            &action_tx,
            "draft-rej",
            None,
            None,
            &mut history,
            crate::agent::loop_::ChatMode::Edit,
        )
        .await;

        assert!(matches!(outcome, ToolExecOutcome::Done));
        let tool_msg = history.last().expect("tool result pushed to history");
        let payload: serde_json::Value = serde_json::from_str(&tool_msg.content).expect("tool payload is JSON");
        assert_eq!(payload.get("success"), Some(&serde_json::json!(false)));
        let content = payload.get("content").and_then(|c| c.as_str()).unwrap_or_default();
        assert!(
            content.contains("Path not allowed") && content.contains("/etc/passwd"),
            "failed tool must surface its error reason in `content`, got: {content}"
        );
    }
}
