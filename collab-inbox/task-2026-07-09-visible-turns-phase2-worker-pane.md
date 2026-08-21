# Task: Visible Turns — Phase 2 (worker/session pane live stream)

- **Date**: 2026-07-09
- **Status**: FINAL(Claude 综合 + Codex 设计第二意见,已定稿)
- **Direction**: worker/session pane 并发。主 chat transcript 保持 **safe-serial**(单 primary 流)。worker pane 点开后显示**该 worker 自己的 live draft 流**。P2 只做渲染/snapshot 接线;运行时仍 safe-serial(真并发 P4);以 reducer 层多 draft(P1 已支持)驱动测试。
- **Base**: `0600f805 feat(chat): keyed reducer stream state for visible turns (Phase 1)`
- **Owner(实现)**: Codex(gpt-5.5/high,goal 模式) / **Audit**: Claude 子进程 + 真机

## 现状与缺口(勘察 + Codex 复核确认)

worker detail pane 通路已存在已接线:`FocusTarget::Worker{sequence}`(focus.rs:48)→ `open_provider_worker_view`(mod.rs:10196)→ `render_active_session_view`(tui.rs:4856)→ `build_provider_worker_active_view_with_io`(action.rs:85)的 `io_lines`,来源 `provider_worker_io_lines_from_conversation(conversation, streaming, 12)`(tui.rs:1305)。

缺口:该 `streaming` 参数在 **4 处**硬编码 `primary_streaming_draft()`/`guard.streaming`,且 P1 的 `visible_drafts`(state.rs:480)未进 `UiSnapshot`(仍只 carry `streaming: Option<StreamingDraft>` state.rs:388)。无论 focus 哪个 worker,pane 都显示同一条 primary。

## 定稿规格

### 1. snapshot 暴露 per-worker draft(精简 view)
```rust
pub struct VisibleStreamingDraftView {
    pub sequence: u64,
    pub draft: StreamingDraft,
}
```
- `UiSnapshot.visible_streaming_drafts: Arc<Vec<VisibleStreamingDraftView>>`;`TuiState` 加同名镜像字段。
- **用 Vec 不用 HashMap**:`visible_drafts` 在 reducer 内已 sequence 升序,Vec 保序、N 小、线性查找即可。
- **不暴露完整 `StreamingTurnDraft`**(勿把 task_id/prompt_preview/reducer 内部形态泄进 TUI;P2 worker pane 只需 sequence + draft)。
- 加 helper 避免调用点手写查找:`fn streaming_draft_for_worker(&self, sequence: u64) -> Option<&StreamingDraft>`,分别放 `StreamState` / `TuiState` / `UiSnapshot` 合适位置。
- 保留 `streaming`(primary)供主 transcript,不动。

### 2. 4 个接入点按 focus 真实 sequence 选 draft
改下列 4 处,按当前 `FocusTarget::Worker{sequence}` 的**真实 worker sequence** 去 `visible_streaming_drafts` 找匹配 draft 喂 io:
- `reduce_provider_worker_status_updated`(state.rs:2456)
- `refresh_provider_worker_view_if_focused`(state.rs:2483)
- `sync_key_mirror_observation_state`(mod.rs:7917)← 草案漏的第 4 点,Codex 补
- `open_provider_worker_view`(mod.rs:10210)

**回退铁律(P2 核心)**:sequence 命中 draft → 用该 draft 构建 live io;**未命中 → `io_lines = Vec::new()`(只显示 worker status/header)**。绝不回退 primary,绝不用 `provider_worker_io_lines_from_conversation(conversation, None, 12)`(它会抽主 transcript 最近 assistant/tool 历史,P3 前无 task 归属,会冒充该 worker 输出)。terminal/awaiting-commit 历史归属留 P3。

### 3. 两个不变量(Codex 补,必守)
- `FocusTarget::Worker{sequence}` 用的是**真实 worker sequence**,**不是** switcher entry 的 synthetic seq(后者带 `PROVIDER_WORKER_SWITCHER_SEQ_BASE = u64::MAX-10_000` 偏移,tui.rs:525;映射见 `provider_worker_sequence_from_switcher_seq` tui.rs:1095)。选 draft 前先还原成真实 sequence。
- **snapshot 路径与 mirror 路径必须一致**:`UiSnapshot` 加 per-worker drafts 后,`sync_key_mirror_observation_state` 要把它同步进 `TuiState`,否则 Ctrl+G / key thread 打开的 worker pane 仍可能只看到 primary 或 stale mirror。

### 4. scroll-anchoring(worker pane append preserve helper)
仅复用 `ActiveSessionView.scroll_offset`(focus.rs:140,"距尾部 offset")不够:离底且新 stream 行 append 时视口会跟尾移动,top row 不稳。**不搬** `FullscreenTranscriptScroll` 整套。加最小 helper 到 active worker view refresh:
- `old_offset == 0` → 追尾,new offset 仍 0。
- `old_offset > 0`(离底)且新 lines 是 append → `new_offset = old_offset + new_len.saturating_sub(old_len)`,再 clamp 到 `max_scroll_offset`。
- 要求 worker view header/status 行高度**固定或有界**,否则 offset 补偿被 header reflow 破坏。
- **不**新增通用 `anchor_top_row` 字段(除非同时重构所有 `ActiveSessionView` 消费方——本 phase 不做)。

## 明确不做(留后续 phase)
- worker strip/switcher 的 per-worker live glyph / elapsed / wtok 扩展(elapsed 已在 worker row 可见;wtok/cost 是 P3)。
- `/queue` 命令(P4 scheduler UX)。
- tool-card 隔离、per-task cost(P3)。
- 真并发运行多 worker(P4)。运行时仍 safe-serial。

## 测试
1. snapshot 暴露按 sequence 可查的 per-worker draft(`streaming_draft_for_worker` 命中/未命中)。
2. focus `w#N` 时 io_lines 来自 sequence==N 的 draft,不是 primary。
3. 两个 draft(reducer 注入,sequence 10/20)→ 分别 focus 各显示自己的流,不串。
4. focus 的 worker 无对应 draft 时 io_lines 为空(不显示别的 worker 流、不抽主 transcript 历史)。
5. **primary 是 A、focus w#B → snapshot 路径与 mirror 路径两条都显示 B**(Codex 指定,防 mirror stale)。
6. scroll offset 在 stream append 期间、离底(old_offset>0)时被正确补偿保留(top row 稳定);在底(==0)时继续追尾。
7. switcher synthetic seq 正确还原为真实 sequence 再选 draft(不因偏移选错/选空)。
8. 主 transcript `streaming`(primary)行为不回归(仍单流)。

## 验收门(Claude 审计核验)
- `fmt --check` / `clippy --all-targets --all-features -D warnings` / `check --all-features` / `check -p openprx --no-default-features` 全绿零 warning。
- 七铁律(零 unwrap/expect 非测试、零 todo、零死代码、无新 `#[allow(dead_code)]` 除非必要并注明)。
- admission guard 未移除;主 transcript safe-serial 不变(primary 路径行为不动)。
- 真机 tmux:mock 驱动,单 worker 真机路径 + reducer 注入多 draft 场景验证 pane 显示正确 worker 的流。
- receipt 交 `collab-outbox/receipt-2026-07-09-visible-turns-phase2.md`,列精确改动文件+行、每测试断言、4 接入点改动、自检输出。
