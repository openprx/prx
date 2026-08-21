# Task: Visible Turns — Phase 3 (task-aware reducer events)

- **Date**: 2026-07-09
- **Status**: FINAL(Claude 综合 + Codex 设计第二意见,已定稿)
- **Direction**: 把 tool 缓冲 / 取消 token / cost·usage 从"当前回合全局"改成"按 task 分桶",消除跨 task 串扰,为 P4 真并发铺路。主 transcript 仍 safe-serial;P3 structural,以 reducer 层多 task 注入驱动测试,运行时不开并发。
- **Base**: `f58bb2bb`(P2)
- **Owner(实现)**: Codex(goal 模式) / **Audit**: Claude 子进程 + 真机
- **拆批(强制,顺序做)**:P3a 数据隔离 → P3b 控制语义 → P3c 计费一致性。三批共享 task_id 主线但失败模式不同,顺序做避免共享文件事故,每批独立 commit + 独立审计。

---

## P3a — Tool buffer task-aware(数据隔离)

### 结构
- 新增 per-task buffer:
  ```rust
  struct TaskToolBuffer {
      pending_tool_cards: Vec<usize>,
      tool_calls: Vec<ToolCallSummary>,
      tool_args: HashMap<ToolInvocationKey, String>,
  }
  ```
- `ControlState` 放 `HashMap<ToolTaskKey, TaskToolBuffer>`,`ToolTaskKey = Task(TurnTaskId) | Primary`(legacy)。
- `pending_tool_cards` 从 `StreamState`(state.rs:502)**移出**到 buffer(它已非 stream 层状态)。
- `ToolCallSummary`(session.rs:33)加 `task_id: Option<u64>` + `sequence: Option<u64>`——仅供持久化/审计,**不作 buffer 主索引**(主索引是 ToolTaskKey)。
- args key `ToolInvocationKey`:理想 `(task_id, tool_call_id)`;若 provider 无稳定 tool_call_id,P3a 至少 `(task_id, name)` + 同 task 内沿用"反向匹配最新 Running 卡片"legacy 行为。**跨 task 必隔离**;同 task 同名并发作为后续增强。

### Action 身份
- `ToolStarted`/`ToolFinished`(action.rs:359/361)加 `task_id: Option<TurnTaskId>`(并预留 `sequence: Option<u64>` / `tool_call_id: Option<String>`)。
- 来源优先级:provider turn driver 从 `StartLLMTurn.provider_turn_task_id` 下传;legacy/main path = None;**不从 primary draft 反推**(最多兼容 fallback)。
- **`RecordAssistantTurn` 必须也 task-aware**(否则最终持久化 assistant turn 时从错误 bucket 取 `tool_calls`)——必查不变量。
- **pending approval 的 task 归属**——必查不变量(勿留交叉污染入口)。

### 终结/清理
- 完成/失败/取消 reducer 只清对应 task 的 buffer,不动其他 task。

### 测试
1. A/B 各有 running tool card → cancel A → B card 仍 running。
2. A 的 tool buffer 清理不影响 B。
3. B 完成时 `tool_calls` 正确写入 B 的 assistant turn(RecordAssistantTurn task-aware)。
4. 跨 task 同名 tool 不互认 args/card。
5. legacy Primary key 路径(单回合)不回归。

---

## P3b — Cancel token task-aware(控制语义)

### 结构
- `ControlState` 加 `turn_cancels: HashMap<TurnTaskId, CancellationToken>`。
- 临时保留 legacy `active_cancel`(state.rs:582);新路径按 task 存取。StartLLMTurn 不再覆盖单槽,而是按 task 存入 map。
- worker registry 继续持 `AbortHandle`(硬杀/生命周期);graceful cancel token 属 reducer 控制语义(保持 `CancelRequested -> Effect::CancelToken` 纯 reducer 可测)。
- 复用 `TurnScheduler::request_cancel(id)`(turn_scheduler.rs:236)作调度层。

### reduce_cancel_requested 修法(不破坏 Ctrl+C 语义)
- `CancelRequested` 仍无参数 = 取消**当前 primary draft 对应 task**。
- 加 helper `cancel_task(task_key)`;未来 `/workers cancel` 或 P4 用 `CancelTaskRequested{task_id}` 走同一 helper。
- 只移除目标 task 的 draft / cancel token / tool cards / tool args/calls。
- **仅当 `visible_drafts` 与 `turn_cancels` 都空**才 `generating=false` + 清全局 approval + 清 legacy slot。

### 测试
1. 两 task token 独立;cancel A 的 token 不动 B。
2. Ctrl+C(CancelRequested)只取消当前 primary task,不清其他 task 缓冲。
3. 都空后才清全局 generating/approval。
4. shutdown 取消全部。
5. 单回合 Ctrl+C 语义不回归。

---

## P3c — Usage task-aware + dedup(计费一致性)

### 结构
- `ProviderUsageRecorded`(action.rs:496)加 `task_id: Option<TurnTaskId>`。
- 加 usage identity:`usage_kind`(至少区分增量 vs FinalAggregate)+ 可选 `event_id/sequence/lease_id`。
- `TurnCompletionSignal.consume_turn_usage(task_id)`(dispatcher.rs:422)定义为 **FinalAggregate**;**只对 final aggregate 按 `(task_id)`(或 `(task_id, lease_id)`)去重一次**。
- **不要用 task_id 去重所有 usage**(否则未来合法多段计量——streaming 分段/retry/tool-loop 多次 provider call——被误丢)。
- 拿不到 lease_id 时,P3c 先做"每 task final aggregate 只 append 一次"的 HashSet;**规格明确:只覆盖 completion double-record,不覆盖未来 incremental metering**。
- `TurnTaskUsageLedger`(turn_scheduler.rs:81,已 per-task 被 `record_usage` 写但只测试读)——P3c 把 task_id plumbing 接通即可;worker row / `/cost` 分 task 展示**留后续**(不在 P3c UX 范围)。

### 测试
1. 同 task 两次完成(重复完成路径)不双计。
2. 乱序完成 usage 归对 task(不错记他 task)。
3. 合法多段计量不被去重误丢(若 P3c 引入 usage_kind,增量段仍累加)。
4. `ProviderUsageRecorded` 无 task_id(legacy)路径不回归。

---

## 明确不做(留 P4/P5)
- 真并发运行多 worker(P4);历史乱序提交(P5);worker strip live token/cost、主 transcript 多块、真并发 streaming UX。运行时仍 safe-serial。

## 每子批验收门(Claude 审计)
- fmt/clippy(-D warnings)/check --all-features/check -p openprx --no-default-features 全绿零 warning;七铁律(零 unwrap/expect 非测试、零 todo、零死代码、无新 allow(dead_code) 除非注明);admission guard 未移除;主 transcript safe-serial primary 路径不回归。
- 真机 tmux:mock 驱动 + reducer 注入多 task 场景验证隔离。
- receipt 交 `collab-outbox/receipt-2026-07-09-visible-turns-phase3{a,b,c}.md`,列精确改动文件+行、每测试断言、必查不变量(RecordAssistantTurn/pending approval task 归属)、自检输出。

## 本次派发:先做 P3a
只做 P3a(tool buffer task-aware)。P3b/P3c 待 P3a 提交后依次派发。
