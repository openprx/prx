# Task 2026-07-11 — Visible Turns P5 (乱序提交完整验证 + demo)

**基线**: HEAD `d99e92ee`（P4c 已提交收口，工作树干净）。**不要 git commit**（提交由主进程做）。**不要改 P4b-2/P4c 生产逻辑**——P5 是**验证与测试加固**期：把乱序有序提交不变量在 N≥3 场景补全，并真机 demo 佐证。仅当验证暴露真实 bug 才改生产码（改了要在 receipt 里单列并给 mutation 证牙）。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。

## 背景
`src/chat/history_commit.rs`（`HistoryCommitCoordinator`：`register_task`/`record_outcome`/`drain_ready`，按 `pending_order`(BTreeSet by sequence) 严格顺序释放 Commit/Skip 决策）现有测试全是 **2-task**：`later_completion_waits_for_earlier_turn`、`cancellation_unblocks_later_commit_without_committing_history`、`failed_turn_skips_and_unblocks_next_turn`、`base_len_mismatch_rejects_stale_outcome`。P4b-2 在 mod.rs 有 finalizer 级 C7/C7a（也是 2-task）。**缺口：N≥3 的乱序 / 混合 cancel+fail+complete 未覆盖**——正是"task#3 先于 #2 完成不损坏 transcript"要证的核心。

## 改动 1 — 协调器 N≥3 单元测试（`history_commit.rs` tests 模块，198-361 一带追加）

补以下用例（每条断言 `drain_ready()` 决策序**严格按 sequence 1→2→3…**、`pending_tasks()`/`pending_outcomes()` 账目、`history_commit_len`/`rollback_to` 正确）：
1. **3-task 纯乱序完成**：register 1,2,3；完成顺序 3→1→2（record_outcome 乱序）。断言：3 先到时 `drain_ready` 空；1 到后仍只能等 2？——**注意**：1 到后 seq1 ready 应立即 Commit seq1，但 seq2 未到 → seq3 仍 blocked。逐步 record 并逐步 drain，验证任何时刻只释放"最小连续已完成前缀"。最终决策序 == [Commit1, Commit2, Commit3]。
2. **3-task 中间 cancel**：seq2 cancelled、seq1/seq3 completed，完成顺序 3→2(cancel)→1。断言最终 drain 序 == [Commit1, Skip2(Cancelled, rollback_to 正确), Commit3]，且 seq3 的 commit_len 反映 seq2 被 skip 后的正确基线（确认 rollback/len 语义在中间 skip 后对 seq3 不串位）。
3. **3-task 首个 fail**：seq1 failed、seq2/seq3 completed。断言 [Skip1(Failed), Commit2, Commit3]。
4. **N=4 混合**：complete/cancel/fail/complete 交错乱序到达，验证严格前缀释放 + 账目归零（`pending_tasks()==0`）。
每条给一处 mutation 直觉说明（如：把 `pending_order` 释放从"最小连续前缀"改成"任意已完成即释放" → 用例变红）。

## 改动 2 — finalizer/event-pump 级 N=3 集成测试（`src/chat/mod.rs`，对标 P4b-2 的 C7/C7a）

在 mod.rs 现有 finalizer 测试旁补一条 N=3：3 个 detached turn，provider 完成顺序 ≠ 派发顺序（如 t3 先完成），经 `finalize_provider_turn_from_event` / `record_provider_history_commit_outcome` / `apply_ready` 路径，断言主 transcript 持久化 Action 序（`RecordUserTurn`/`RecordAssistantTurn`/`StreamCompleted` 或等价）**按 task 派发序**落盘、无重复/无错序/无丢轮。复用 P4b-2 已有的 `pending_ordered_provider_turn_commits` 跨迭代 map 机制。给 mutation 证牙。

## 改动 3 — 真机 PTY demo（N=3 乱序，佐证不损坏）
**只走 PTY/tmux demo，绝不碰 systemd `prx.service` 生产 daemon**（部署到生产是用户决策门，不在本单范围）。
- debug build `prx`（--all-features）。
- 配置 `[chat].max_concurrent_visible_turns=3`（demo config，默认仍是 2）。
- `OPENPRX_MOCK_DELAY_MS_BY_PROMPT` 令三个 prompt 完成顺序 ≠ 派发顺序（如 `alpha=15000;bravo=3000;charlie=8000` → 完成序 bravo→charlie→alpha，派发序 alpha→bravo→charlie）。
- tmux 驱动派 3 个 detached turn，`/workers` 佐证 3 个同时 running，等全部完成后 dump 持久化 session transcript，断言 user/assistant 对**按派发序 alpha,bravo,charlie 排列**、恰好 3 组、无损坏。贴关键截录。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-visible-turns-phase5.md`，别 commit）
逐条实跑贴结果：
1. `cargo fmt --check`
2. `cargo clippy -p openprx --all-targets --all-features -- -D warnings`
3. `cargo check -p openprx --all-features` + `--no-default-features`
4. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 `N passed; M failed`（预期 ≥5473 + 新增，0 failed）
5. 新增测试逐个列名 + 关键用例 mutation 证牙（改协调器释放逻辑/finalizer 排序为 bug → 变红）
6. 真机 demo：`/workers` 3 并发截录 + 最终 transcript 有序性证据；跑不动说清卡点别假装
7. 明确写：**未 commit**、是否触及生产码（应否；若碰了单列 + 理由 + 证牙）、demo 结果

铁律：零 unwrap/expect（生产码，测试可用 expect）、零 warning、English 代码/commit。别自 commit、别碰 P4b-2/P4c 逻辑、别动生产 daemon。
