# Task: Visible Provider Turns — Phase 1 (keyed reducer stream state)

- **Date**: 2026-07-09
- **Mode**: goal(一次交付完整目标)
- **Owner(实现)**: Codex(gpt-5.5/high)
- **Audit(审计)**: Claude 子进程 + 真机 build/test
- **Depends on**: `collab-outbox/receipt-2026-07-09-visible-provider-turns-scope-audit.md`(已 Claude 独立核验 13/13 属实,已 Codex 交叉审计定稿)

## 背景与边界

当前 visible provider turn 是**有意的 safe-serial**:`StreamState` 单 `draft` 槽,主循环 await 当前 turn 完成才回外层,admission guard(`active_workers == 0`)兜底。Phase 1 只做**结构地基**(keyed 流状态),**不放开 admission guard**,**不声称用户可见并发流式**。此地基对两个产品方向(主 transcript 多流 / worker-pane 并发)都必需,方向无关。

⚠️ **产品岔路(Phase 2+ 才需决策,Phase 1 不阻塞)**:主 transcript 内多流 vs 主 transcript safe-serial + worker/session pane 显示并发。Phase 1 先不预设。

## Phase 1 定稿规格(Codex 第二意见已并入)

### 数据结构
```rust
pub struct StreamingTurnDraft {
    pub task_id: Option<TurnTaskId>,
    pub sequence: u64,          // scheduler sequence,决定可见顺序(非到达顺序)
    pub prompt_preview: String,
    pub draft: StreamingDraft,
}
```
- `StreamState.visible_drafts: Vec<StreamingTurnDraft>` —— **单一数据源**。
- **用 `Vec` 不用 `IndexMap`**:Cargo.toml 无 direct indexmap 依赖;N 很小;顺序即核心语义,按 sequence 插入/排序。将来需 O(1) 再加 `HashMap<DraftId, usize>` 或正式引入 IndexMap。

### 兼容层(关键:避免双写不一致)
- **停止写入** `StreamState.draft`(移除该字段或改为不再赋值)。
- 提供 `primary_draft() -> Option<&StreamingTurnDraft>` / `primary_draft_mut()`(primary = sequence 最小的活跃 draft,即当前 safe-serial 下唯一那个)。
- `UiSnapshot.streaming` 临时由 `stream.primary_draft().map(|d| d.draft.clone())` 计算,保旧 snapshot/renderer/tests 兼容。
- **所有 reducer 流更新只写 `visible_drafts` 单源**,不得再有 draft 与 visible_drafts 两套状态。

### 遗漏边界(Codex 补,必做)
1. **dirty fingerprint**:`snapshot_dirty_fields` 现依赖 `draft_version`/`has_stream_draft`,多 draft 后必须聚合(draft count + max/sum version,必要时 active draft ids hash),否则 snapshot 漏刷新。
2. **Cancel/Failed 终止路径**:reducer 对 cancel/failed 也要按 draft id 精确作用,不清其他 draft。
3. **admission guard 保留**:Phase 1 不迁移 tool buffers,必须保留 guard,runtime 不得进入多工具并发;代码/测试注明 "structural only, not live visible concurrency"。
4. **non-TUI parity**:`cargo check -p openprx --no-default-features` 必须过;keyed draft helper 在 non-TUI 下也编译(注意 state.rs 的 non-terminal placeholder StreamingDraft)。

### 测试(4 → 7)
1. two drafts start without overwriting
2. chunk routes by draft id
3. completing A leaves B active
4. stale chunk for completed A ignored
5. cancelling A leaves B active
6. failing A leaves B active
7. snapshot dirty changes when non-primary draft version changes

## 顺带(Codex 已答应)
把以下写进 `receipt-...-scope-audit.md`,作为活契约:产品 gate、Phase 2 scroll anchor 验收(底部 follow / 离底保 top anchor / 稳定 block id / preview 高度上限 / focus 不被抢)、Phase 3 per-task cost(usage accumulator / task-aware ProviderUsageRecorded / worker row 显示 token / /cost 只算 finalized / 乱序不重复计费)、Phase 3 cancel/tool-card isolation 用例(A/B 都有 running card → cancel A → B card 仍 running → B 完成 tool_calls 正确写入 B)。

## 验收门(Claude 审计核验)
- `cargo fmt --all --check` / `cargo clippy --all-targets --all-features -D warnings` / `cargo check --all-features` / `cargo check -p openprx --no-default-features` 全绿零 warning。
- 7 个测试全过。
- 零 unwrap/expect(非测试)、零 todo!/unimplemented!、零死代码(七条铁律)。
- admission guard **未** 被移除(grep 确认 `provider_turn_visible_admission` 仍在调用路径)。
- receipt 交回 `collab-outbox/receipt-2026-07-09-visible-turns-phase1.md`,列出精确改动文件+行、每个测试的断言、自检输出。
