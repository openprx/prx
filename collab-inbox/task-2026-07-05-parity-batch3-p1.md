# [派工] Parity 战役 Batch 3（P1 批 F6-F12 + 跟进包）

前置：P0 全部 5 项已 ACCEPT（F1/F2/F4/F5 + F3 经两轮返工）。**收到本文档配套的 tmux 放行通知后才开工**。仍然禁止 push。总计划 `/opt/worker/task/prx/fix-plan-parity-gap-2026-07-05.md`，审计报告 `/opt/worker/report/prx-chat-parity-audit-2026-07-05.md`（各条含 file:line 证据）。

## Batch 3a（先做，写 receipt-...-batch3a.md）

- **F6 (TUI-3+TUI-4)**: 工具卡体验：①折叠态在统计行下追加**结果前 2-3 行内容预览** + `… +N lines` 尾注（复用 clamp 家族，宽度感知）；②Ctrl+O 改真 verbose：全局 expand-all 开关作用于 transcript 本体（工具卡/reasoning 卡全文可滚动），替代现 lossy /transcript viewer 或让 viewer 输出完整内容；③放开展开态 40 行/240 字符硬截（至少 verbose 模式下全文）。
- **F7 (INPUT-4)**: 多行输入链路：①TerminalGuard 进入时 `PushKeyboardEnhancementFlags`（先 `supports_keyboard_enhancement` 能力探测，不支持则跳过，退出对称 Pop）；②`\`+Enter 续行（Enter 时当前行尾为 `\` → 删反斜杠+插换行，全终端通用）；③footer/帮助提示换行键位。
- **F8 (TOK-5+TOK-4)**: 压缩语义对齐：①`/compact` 与 overflow 兜底改走 `apply_configurable_compaction`（LLM 结构化摘要），失败/超时降级现 char-trim；②reducer mirror（state.rs:2384 区 compact 算法）同步；③自动压缩（preflight+overflow）完成后 dispatch SystemMessageAdded（复用 format_compact_feedback 的前后对比文案），不再静默；④`/compact` 后补发 ContextWindowUpdated 让 ctx% 立刻回落（上轮 F5 非阻塞项）。

## Batch 3b（3a receipt 后继续，写 receipt-...-batch3b.md）

- **F9 (TOK-2)**: Anthropic cache token 入账：AnthropicUsage 解析 `cache_creation_input_tokens`/`cache_read_input_tokens` → TokenUsage/MeteredTokenUsageRecord 透传 → ModelPricing 加 cache_read/cache_write 价 → usage_cost_usd 计入。显示层 total 口径写清（cache 部分算入 prompt 侧）。
- **F10 (SESS-1+INPUT-10)**: strip 选中一致性：①reap 命中当前 strip_selection 时清 TuiState.strip_selection + dispatch StripSelectionChanged{None}（Redux 同步）；②Alt+Enter 在 selection 悬空/未命中时返回 Consumed + 状态行提示 "session gone"，绝不 fall-through 插换行；③Redux reducer 补 strip-selection/Alt+Enter attach 分支（双路径 parity + 测试）。
- **F11 (SESS-2)**: 工具层 shell 子进程治理：src/tools/shell.rs 的 Command 加 `kill_on_drop(true)`（或对齐 chat shell 的 pgid+显式 kill），使 agent abort/`/quit` 传递到 OS 进程；核查 src/tools/ 下其他 spawn 点一并处理。
- **F12 (TUI-6)**: 滚动锚定：offset_from_bottom>0 时改内容锚定（记首个可见行/距顶行数冻结），新输出不再推走阅读位置；End/滚到底恢复 follow 模式。

## Batch 3c（跟进包，LOW，可并入 3b receipt 或单独）

1. **inline bold/italic 渲染**：`render_inline_markdown`（renderer.rs）文档写 bold/italic/code 实现只有 code——补 `**bold**`→加粗、`*italic*`/`_italic_`→斜体（E2E 真机发现，TUI-1 残余）。
2. **spinner wall-clock tick**：用已有 50ms idle poll 捎带 tick，等待期/工具期 glyph 真转（F3 已申报偏离的补完）。
3. **F4 性能跟进**：①渲染缓存按消息 seq 键控或容量自适应（>128 条 assistant 消息 FIFO thrash，20fps 全史重渲）；②流式 markdown 尺寸阈值/节流（实测 50KB 代码块 114ms/帧）；③桥补 38;5 索引色 + 非 'm' CSI 跳过；④markdown 缓存测试的全局态并行 flake 窗口（独立实例或 serial）。
5. **F2 跟进**：`slash_menu_only_triggers_at_first_line_start` 补牙（用第二行 `/he` 有匹配 filter）；`slash_menu_overlay_rect` 几何测试；删零调用空源 helper（tui.rs:661/830 区）+ 不可达 "No matching" 渲染段；`/save` 后刷新 saved_sessions 缓存；启动双次 `saved_chat_sessions()` 合并 + Err 加 warn。
6. **F1 跟进**：wiring 级测试（surface_turn_elapsed_message 直连 fallback 会红）。
7. **测试 flake 根治（可选）**：providers env 锁跨模块共享态、landlock pid 目录竞争（上轮复审已定位根因，与 parity 无关，可独立小修）。
8. **signal 路径 mirror 清理（LOW）**：外部 `kill -INT`（mod.rs:2866 signal handler 的 CancelRequested）不清 approval mirror——非键盘可达的极端组合，补一行 clear 即可对称。

## 验收门（同 Batch 1，逐条咬合）

双路径 parity / plain no-render 测试 / GP-8 字面锚 / `cargo fmt --check` + clippy --all-targets -D warnings + test --bin prx / receipt 写明二进制路径与偏离。F8 涉及 LLM 调用的降级路径要有超时测试；F11 要有"abort 后子进程确实死了"的验证手段（哪怕 ignored 标注的真机测试）。
