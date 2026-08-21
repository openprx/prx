# Task: Visible Turns — Phase 4 (event-driven visible scheduler + real concurrency)

- **Date**: 2026-07-09
- **Status**: FINAL(Claude 综合 + Codex 设计第二意见,已定稿)
- **Direction**: 主循环改事件驱动,放开 admission 到并发上限跑多 Redux visible worker。**主 transcript 仍 safe-serial 单 primary**,并发详情在 worker pane。**运行时行为第一次改变**。
- **Base**: `cbde09ce`(P3c)
- **Owner(实现)**: Codex / **Audit**: Claude 子进程 + 真机
- **拆批(强制,顺序做)**:P4a 事件泵骨架(N=1)→ P4b-1 ordered commit gate(N=1)→ P4b-2 放开并发(N)→ P4c UX。**关键顺序铁律:persistence 保序 gate(P4b-1)必须在放开并发(P4b-2)之前,否则打开最危险的乱序写窗口。**

## 现状与硬骨头(勘察 + Codex 复核)

- 外层 loop(mod.rs:4049/4107)与 Redux turn-await(6555/6601)已是 select! 事件驱动;串行根 = per-turn body(6009–7650)顺序 await。
- admission `can_start_visible = active_workers == 0`(mod.rs:8438/8455 硬编码)。
- 地基 ready:`TurnScheduler`(多派发/优先级 next_queued_index)、`TurnCompletionSignal` keyed slots + `spawn_provider_turn_completion_waiter` + `provider_completion_rx`、`HistoryCommitCoordinator.drain_ready`、worker registry(request_cancel/abort_execution by task_id)、P3a/b/c per-task 三件。
- **硬骨头**:①driver 的 `RecordAssistantTurn→StreamCompleted→SaveSession` 在 dispatcher 子任务(dispatcher.rs:2220 / state.rs:1796)**按完成顺序**发生,非 sequence;canonical history.push / chat_session.add_* 散在 Redux 分支后半(mod.rs:6815/6864)。②Legacy `run_tool_call_loop_traced(&mut history)`(7021)就地借用无法并发。

---

## P4a — 事件泵骨架(保持 N=1,纯结构重构)

**最小切口(Codex 定,勿一刀删 6555)**:
1. 抽共享 **event pump** 函数:统一处理 `input_rx`(enqueue/local command/cancel)、`provider_completion_rx`(route completion)、`provider_turn_lifecycle_rx`、`shutdown`。
2. 让 6555 内层 await 调该 pump,**仍只等当前 turn 完成**,N=1 严格等价。
3. 再把 Redux turn 启动后注册进 `pending_redux_turns` map、回外层 loop,由外层 completion arm 做 finalizer。admission 仍 `active_workers==0`——只改"谁消费事件",不改"能否启动第二个"。

**等价不变量(必守)**:admission 仍阻塞 active worker(8438);6555 内层现吞的 input/cancel/lifecycle arm 语义必须保留(6555/6601);外层已有 completion/lifecycle/input select(4049/4107)。

**测试**:N=1 下单 turn / 连续 turn / tool turn / cancel / 完成落盘,与 P3c 前**行为等价**(现有 parity/PTY 全绿 + 新增等价性单测)。

---

## P4b-1 — PerTurnContext + ordered commit gate(保持 N=1)

**PerTurnContext**(按 TurnTaskId 放 chat::run 栈局部 map,靠近 `provider_turn_completion_contexts` mod.rs:4004),字段:
- identity:task_id / sequence / draft_id / worker kind
- prompt·history:user_input / enriched / system_prompt / history_base_len / history_len_before_user_turn / history_len_before_assistant / history_snapshot
- runtime IDs:turn_run_id / chat_user_event / runtime_envelope·session key / route_decision / route_scope / provider_started_at
- control:per-task cancel token / spawn·message-send ctx / draft·finalizer handles
- final payload:terminal_plan / completed·failed·cancelled payload / usage / recorded response

**ordered commit gate 接线**(改 `drain_provider_turn_finalizer_events_and_publish` 8896 之外的业务提交):
- completion 到 → 生成 terminal payload,记 outcome 到 `HistoryCommitCoordinator`。
- `drain_ready()` 返回 ready decisions。
- 每个 ready decision:从 PerTurnContext 取 payload,**按 sequence** ordered commit。
- **ordered commit 才 dispatch RecordAssistantTurn/StreamCompleted,才允许 reducer 产生 SaveSession**。failed/cancelled 走 ordered skip,清 draft/worker,**不写 assistant/session**。
- 实现二选一:**新增 `CommitProviderTurn` action** 把 RecordAssistantTurn+StreamCompleted+SaveSession 收在 commit-ready 后;或让 dispatcher driver 不再直接发触发 SaveSession 的终止 action。
- **保持 N=1**:本批不放开 admission,只把 persistence 收口到 gate。行为等价(N=1 下 drain_ready 直通)。

**测试**:N=1 下 completion→gate→ordered commit 与 P4a 行为等价;coordinator skip 路径(failed/cancelled)不写 session;reducer SaveSession 只在 commit-ready 后发。

---

## P4b-2 — 放开并发上限(N)

- 新增配置 `chat.max_concurrent_visible_turns`(**默认 2**,owner 决策 2026-07-10:并发默认开)。
- **统一 admission 策略函数**:
  - 请求 ForegroundAwaited(Legacy):只在 `active_workers==0` 起;任何 Legacy active → 阻止所有新 visible turn。
  - Redux detached:只在**无 legacy active** 且 `detached_active < max_concurrent_visible_turns` 起。
  - N>1 只对全局 ReduxDriver 模式开;Legacy/非 TUI/无 draft 路径 effective max=1。route 落到 Legacy 时 assert 当前无 active worker,否则 requeue 不启动。
- 改 admission 3 处(结构字段 mod.rs:216 / 函数 8455 / 调用点 8328)。
- history:**每 task 一份 `history.clone()` 给 provider 执行;canonical history 只在 coordinator ready 后按 sequence 更新**——不共享 `&mut`,不整体覆盖回;mid-turn compaction 只作用 task-local snapshot,durable 变化变成 ordered delta 再提交。
- **⚠️ 必备前置(P4b-1 审计 Finding 3)**:P4b-1 的 ordered gate 里,若 `finalized==false`(N>1 延迟轮:earlier task 未完成时后者 block),`dispatch_ordered_provider_turn_commit` 不执行 → `RecordAssistantTurn`/`StreamCompleted` **永不下发** → reducer draft 永久悬挂 + 该轮不落 session + 流式文本丢弃。**P4b-2 放开并发后,非 finalized 的延迟轮必须在其 earlier task 收口、coordinator 后续 `drain_ready()` 把它变 ready 时补发 ordered commit(RecordAssistantTurn→StreamCompleted)**,否则 draft 泄漏 + 丢轮。这是 N>1 正确性的核心,必须有测试:task#3 先于 #2 完成 → #3 的 commit 延迟到 #2 收口后才落库,期间 #3 draft 不泄漏、最终正确落 session。
- P4b-1 已把仅供 debug 的投机字段从 PerTurnContext 移除;P4b-2 真正需要的 per-task 字段(history 视图等)在此按需加回并接真实消费逻辑。

**测试**:N=2 两 Redux turn 并发;task#3 先于 #2 完成走 coordinator 不损坏 history;SaveSession 有序;Legacy turn 期间不派发第二 visible;并发上限生效。

**真机(必须真二进制 tmux)**:mock provider prompt A 延迟 5s / B 延迟 1s(用 OPENPRX_MOCK_SCRIPT 或延迟机制),`max_concurrent_visible_turns=2`,tmux 快速发两条 visible turn,`/workers` 或 pane capture 证明 w#1/w#2 同时 running,再证明主 transcript/session 保存序仍 A 后 B。

---

## P4c — UX 收尾(低风险)

- `/queue`(mod.rs:9279)改读 `TurnScheduler.status()`/`queued_preview`,区分 queued vs running(现只报 input_backlog)。
- `/workers cancel w#N`(mod.rs:9352)删 foreground 限制(9389–9398),对任意 worker 派发 cancel(P3b per-task token 已支持)。
- 优先级派发对齐(scheduler `next_queued_index` 已实现,只影响排队不动已运行)。

**测试**:/queue 反映 queued vs running;cancel 非前台 worker 只取消该 worker;priority 排队顺序;都走 PTY。

---

## 每子批验收门
- fmt/clippy(-D warnings)/check --all-features/check -p openprx --no-default-features 全绿零 warning;七铁律(**零死码,注意 crate 级 allow 掩盖**);主 transcript safe-serial 单 primary 不回归。
- 真机:P4a/P4b-1/P4c 走 PTY(不碰 cargo build);**P4b-2 需真二进制 tmux demo 两 turn 并发**(到时按铁律5 请示 build 方式)。
- receipt 交 collab-outbox/receipt-...-phase4{a,b1,b2,c}.md,列精确改动+行、每测试断言、等价性/保序如何证明、自检输出。

## 本次派发:先做 P4a
只做 P4a(event pump 骨架,N=1 严格等价)。P4b-1/b-2/c 待 P4a 提交后依次派发。
