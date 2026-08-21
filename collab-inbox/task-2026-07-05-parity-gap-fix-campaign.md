# [派工] Parity 差距修复战役（round-3，P0→P1→P2 全量）

- **来源**: 审计报告 `/opt/worker/report/prx-chat-parity-audit-2026-07-05.md`（5 维审计 + 你方 gpt-5.5 交叉审计，HIGH 16/16 CONFIRMED）
- **总计划**: `/opt/worker/task/prx/fix-plan-parity-gap-2026-07-05.md`（F 编号与本文档一致，每条含 file:line 证据与修法）
- **基线**: main HEAD `c4397f4b`（v0.7.7）
- **🔴 铁令：全程禁止 push**（用户明确：代码先不用 push）。只 commit 到本地 main。
- Commit message 英文、不加 Co-Authored-By、一个 F 项一个 commit（英文写 "fix(chat): ..." 风格）。

## 批次结构

### Batch 1（先做）：F1 + F2 + F3 + F5
- **F1 (INV-1)**: plain/非-TTY 泄漏 `turn completed Ns`。`surface_turn_elapsed_message`（mod.rs:1066-1080）在 fallback 路径（redraw_tx=None）短路不打印；补 `plain_mode_suppresses_turn_elapsed_chrome` 测试。顺带评估 CDX-1（plain banner mod.rs:2452/2763）：若 banner 属既有约定则不动并在 receipt 说明，若认定污染则同 commit 清理。
- **F2 (INV-2 + INPUT-2 + INPUT-11)**: slash 菜单一揽子：
  1. reducer 六处空 `SlashMenuSources`（state.rs:839/895/916/946/988/1013）改真实源——catalog/saved_sessions 经 Action 装填进 UiState（类比 SessionsEntriesUpdated），六处引用真实源；补"两路径同 key 序列产出相同 slash_menu"parity 测试。
  2. 巨型居中遮罩（tui.rs:3246 92%×85%）改**输入框上方锚定的紧凑下拉**（高度 = min(条目数,10)+hint，不遮对话）。
  3. Enter：无参命令直接执行（Submitted）；带参命令 = 补全进入参数位；Tab 保持只补全。
  4. 无匹配时菜单收起（置 None），不弹 "No matching" 遮罩。
  5. `/` 触发限首行首字符（修 tui.rs:1457 多行 `/usr/...` 误触发）。
  6. INPUT-11：过滤改 name+description 统一匹配、refresh 后 selected 重置为 0。
- **F3 (INPUT-1 + TUI-2 + CDX-2 + INPUT-5)**: "生成中可控"闭环：
  1. Esc 生成中中断：`resolve_esc`（focus.rs:362-377）加 generating 维度 → 复用 `Action::CancelRequested` 管线；mod.rs:7846 不再丢弃 Cancelled。非生成态 Esc 语义（清输入/清选中/detach）保持不变。
  2. 状态行活动指示：spinner 帧 + 回合 elapsed + "(esc to interrupt)"，用已有 1s sessions tick 驱动；工具 Running 卡 `●` 轮转。
  3. CDX-2：Pure reducer 路径 ToolProgress 可见（state.rs:4133 只 redraw/log、7075 dirty=false → 修成产生可见状态）。
  4. INPUT-5：Ctrl+C（+空输入 Ctrl+D）提到所有 overlay 分支之前，slash 菜单/switcher/picker 打开时不得吞。
- **F5 (TOK-1)**: 状态栏加 `ctx:XX%`（`plan_context_budget` loop_.rs:994-1019 + `ui.context_window_tokens` state.rs:308 原料齐备）；估算源（Heuristic）加 `~` 前缀（守"无假 token"）；85% 后接近满时可变色/加警示。原每 turn 重发的系统消息警告（mod.rs:427-447）降频或撤销（状态栏常驻后不需要刷屏）。

### Batch 2（Batch 1 receipt 写完立即开工，不等审计）：F4
- **F4 (TUI-1)**: markdown + syntect 接入全屏渲染路径。renderer.rs:354 `render_markdown_with_highlighting` 已有，做 ANSI→ratatui Spans 桥（或直接 markdown→Spans 转换层），接 `render_conversation_line` 的 Assistant + StreamingAssistant 两分支（tui.rs:4296-4324）。流式期间行级增量解析可接受（完整重解析 finalize 后做）。注意 TUI-8 性能背景：别让每帧全量 markdown 重解析雪上加霜——已 finalize 消息的渲染结果要缓存。

### Batch 3+（🔴 等 collab-inbox 出现 Batch1/2 审计 acceptance 文档后才开工）：P1 批 F6-F12
按总计划顺序：F6 工具卡预览+Ctrl+O verbose、F7 键盘增强+`\`续行、F8 /compact 摘要化+压缩可见（注意 reducer mirror state.rs:2384 同步）、F9 Anthropic cache token、F10 SESS-1+INPUT-10 strip_selection、F11 kill_on_drop、F12 滚动内容锚定。P2 批再往后，同样等 acceptance。

## 每个 F 项验收门（审计会逐条咬合验证）

1. **双路径 parity**：legacy mirror + Redux reducer 同改，行为一致（log_redux_key_diff 不新增分歧）。
2. **plain no-render**：所有新增 chrome（spinner/ctx%/菜单变化）必须有 `--plain` 不渲染测试。
3. **GP-8 有牙断言**：负向守卫断字面锚（如 `== 200_000`），禁 `*::default().field`/常量自比。
4. **自检**：`cargo fmt --all -- --check` + `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` + 相关 `cargo test --bin prx`。
5. 构建用你自己的 CARGO_TARGET_DIR 并在 receipt 写明二进制路径（真机验证要用）。

## Receipt 约定

每个 Batch 完成写 `collab-outbox/receipt-2026-07-05-parity-batch<N>.md`：commit hash 列表（每 F 一行）、新增测试名、自检结果、二进制路径、未尽事项/偏离说明。tmux 只发一行通知，payload 全在文档。
