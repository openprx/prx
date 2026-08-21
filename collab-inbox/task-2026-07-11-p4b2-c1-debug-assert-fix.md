# Task 2026-07-11 — P4b-2 收口: 修 C1 矛盾 debug_assert (+尽力补 T1 集成测试)

**基线**: 当前工作树（P4b-2 未提交实现，HEAD `bc75fad9`，改动 `src/chat/mod.rs`/`src/config/schema.rs`/`src/providers/router.rs`）。**不要 git commit**（提交由主进程做）。**不要动 P4b-2 其余逻辑**——它已通过独立审计 + 全量 test(5468 passed/0 failed) + 真机 demo(2 worker 并发/乱序有序提交/CPU 0.3% 无 livelock)。本单只做 C1 修复与 T1 补测。

## C1（必做）— 删除自相矛盾的 `debug_assert!`

位置 `src/chat/mod.rs:6741-6748`（post-route 准入拒绝分支内）：

```rust
if !provider_admission.can_start_visible {
    debug_assert!(
        !matches!(
            provider_worker_kind,
            crate::chat::turn_worker::ProviderTurnWorkerKind::ForegroundAwaited
        ) || provider_admission.active_workers == 0,
        "Legacy provider turn routed while another visible provider turn is active"
    );
    tracing::warn!( ... "visible provider turn admission rejected after route decision; requeueing input" );
    input_backlog.push_front(...);
    defer_visible_input_pop_once = true;
    ...
}
```

**问题（已由独立审计 + 主进程 Read 源码双重确认）**：进入此分支的前提是 `!can_start_visible`。对 `ForegroundAwaited`，准入函数（mod.rs:8817）`can_start_visible == (active_workers == 0)`，故进分支 ⟺ `active_workers != 0`。此时若 `kind == ForegroundAwaited`，debug_assert 两个析取项 `!matches!(..ForegroundAwaited)` 与 `active_workers == 0` **恒同时为 false → debug 构建必 panic**。而这恰是下方 requeue 逻辑专门优雅处理的合法场景（TUI 下 draft 发送失败 → ForegroundAwaited + 有并发 detached worker）。断言与它自己下面的兜底代码自相矛盾。release 编译掉不影响生产，但 `cargo run`/debug 二进制命中即崩。

**修法**：直接**删除** `debug_assert!(...)` 整段（6742-6748）。下方 `tracing::warn!` 已完整记录该拒绝的所有字段，断言纯属画蛇添足且逻辑错误。不要试图"修正"断言条件——这条路径本就是合法可达的，不该 assert。

## T1（尽力，非阻塞）— 集成测试锁 livelock 断环回归

现有测试 `post_route_requeue_defers_next_visible_pop_once_under_detached_capacity`（mod.rs:16787 附近）是**手动**置 `defer_visible_input_pop_once = true` 后测一次性语义，**没走真实的 "post-route 准入拒绝 → 置 flag → 事件泵跳过一次 pop" 集成链路**。即：若 mutation 让 6762 的置 flag 漏掉、或 requeue 漏掉，该测试不会变红（假牙风险）。

**目标**：补一条真正驱动 per-turn body 走到该 requeue 分支（default N=2、有并发 detached worker 占位、新 turn 被 post-route 拒绝并 requeue+defer）的集成测试，使 mutation 掉 requeue/defer 后变红。

**若该集成路径因现有 harness（需构造 draft 发送失败 + 并发 detached 占位）无法在合理工作量内驱动**：不要硬造假测试，改为在 receipt 里说明卡点、保留现有单测，把 T1 记为已知覆盖缺口即可。C1 是硬要求，T1 尽力。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-p4b2-c1-fix.md`，别 commit）

在 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp` 下逐条实跑并贴结果：
1. `cargo fmt --check`
2. `cargo clippy -p openprx --all-targets --all-features -- -D warnings`（必真跑 clippy）
3. `cargo check -p openprx --all-features` + `cargo check -p openprx --no-default-features`
4. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 `N passed; M failed`（预期仍 ≥5468 passed / 0 failed；若补了 T1 则 +1）
5. 若补了 T1：贴 mutation 证明（把 requeue 或 defer 置 flag 改回 bug，该测试变红的证据）
6. 明确写：**未 commit、C1 已删、T1 状态（补了/记为缺口+原因）**
