# Task 2026-07-11 — Visible Turns P4c (低风险 UX)

**基线**: HEAD `f88ec4d6`（P4b-2 已提交收口，工作树干净）。**不要 git commit**（提交由主进程做）。**不要动 P4b-2 的准入/ordered-commit/livelock 逻辑**——只做本单三项 UX 对齐。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。

P4b-2 之后主 transcript 可有并发 detached worker 在跑，但两个面向用户的命令还停留在"单前台 turn"世界观，需对齐。

## 改动 1 — `/queue` 分 queued / running

**现状**：`/queue` 走 `format_input_backlog_report`（`src/chat/mod.rs:10562`），只报输入 backlog 的 `{queued} queued, {priority} priority`，**不反映正在跑的 provider turn**。并发下用户看不到"有几个 turn 在 running"。

**目标**：`/queue` 输出同时体现 **queued（含 priority）** 与 **running**。数据源用 `TurnScheduler::status()`（`src/chat/turn_scheduler.rs:341`，`TurnSchedulerStatus{ queued, priority_queued, running }` 已就绪）。

**做法建议**：让 `/queue` handler（`is_queue_command` 消费处，约 `src/chat/mod.rs:10213` 一带；`format_input_backlog_report` 调用点）把 scheduler 传进来，报头行改为类似：
```
Main queue: 2 queued (1 priority), 2 running.
```
running 取 `scheduler.status().running`。backlog 明细列表逻辑保留不动。
- ⚠️ 别破坏既有断言：`/queue` 相关测试在 `mod.rs:15882/15929/18086` 一带；如报头文案变了，**同步更新这些测试**（不是删断言，是改成新文案 + 新增对 running 计数的断言）。
- 保持 `input_backlog_status` / `MainQueueStatus`（TUI 状态栏用）不变，只扩 `/queue` 的文本报告。

## 改动 2 — `/workers cancel w#N` 放开取消任意 worker

**现状**：`handle_workers_cancel`（`src/chat/mod.rs:10401-10453`）在 10425-10434 有硬门 `is_current_foreground = provider_turn_task_id == Some(worker.task_id)`，非当前前台 turn 一律拒绝（`"... is not the foreground awaited turn yet."`）。P3b 已给**每个 task 独立 cancel token**、P4b-2 有并发 detached worker，此限制已过时——用户无法取消一个正在跑的 detached worker。

**目标**：删掉 `is_current_foreground` 门（10425-10434 整段 early-return）。只要 worker 处于 `Running | Cancelling`（10412-10424 的状态守卫**保留**），就用它自己的 `task_id` 调 `request_provider_turn_cancel(scheduler, workers, Some(worker.task_id), "workers cancel command")` 取消——per-task token 保证只取消目标 turn、不误伤其它并发 turn。成功消息已含 `kind={detached|foreground_awaited}`（10447），取消 detached 会正确显示 kind=detached。
- 保留：worker 不存在（10406-10411）、已终态（10417-10424）两个 early-return 不变。
- ⚠️ 既有测试 `mod.rs:15846/15862/15880` 一带假设"只有当前前台可取消"；放开后 detached 也可取消——**更新这些测试** + **新增**一条：并发两个 detached worker 时 `/workers cancel w#2` 只取消 w#2、w#1 仍 running（用 per-task token 语义断言）。

## 改动 3 — 优先级派发对齐（验证 + 必要时修）

P4b-2 并发准入放行后，确认 **priority 队列任务在并发可见 slot 空出时优先于 normal 被派发**（scheduler 的 priority 抢占 `turn_scheduler.rs` 既有语义，与新 detached 准入 `detached_active < max` 的交互）。

**做法**：读 `pop_next_visible_input_task_with_scheduler` / `pop_next_input_task_with_scheduler` 与 scheduler 的 `next_queued_index`（priority 优先）交互。若已正确，加一条集成测试锁死"N=2、1 running + backlog 有 [normal, priority] 两条 → 下一个被派发的是 priority"；若发现 priority 在并发路径下被跳过/乱序，**修**并说明。不要为凑改动伪造问题——若确实已对齐，测试通过即为交付，receipt 写明"已对齐无需改码"。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-visible-turns-phase4c.md`，别 commit）

逐条实跑贴结果：
1. `cargo fmt --check`
2. `cargo clippy -p openprx --all-targets --all-features -- -D warnings`
3. `cargo check -p openprx --all-features` + `--no-default-features`
4. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 `N passed; M failed`（预期 ≥5469 + 新增测试，0 failed）
5. 新增/改动测试逐个列名；改动 2/3 的关键新测试给 mutation 证牙（把放开逻辑或 priority 选择改回 bug → 变红）
6. **真机 PTY 验证**（对标 P4b-2 demo 方法，`OPENPRX_MOCK_DELAY_MS_BY_PROMPT` + tmux 驱动 `prx chat`）：
   - `/queue` 在有 running turn 时正确显示 `X running`
   - 起 2 个并发 detached worker，`/workers cancel w#2` 成功取消 w#2 且 w#1 继续 running（贴 `/workers` 前后输出）
   贴关键终端截录；跑不动就说清卡点，别假装通过。
7. 明确写：**未 commit**、三项改动各自状态、真机验证结果。

铁律：零 unwrap/expect（生产码）、零 warning、English 代码注释与 commit 文案。别自 commit、别碰 P4b-2 逻辑。
