# [派工] Chat UX 第三轮：demo 真机测试用户反馈 + 打磨项（对标 Claude Code）

来源：2026-07-06 demo 真机测试（binary `3c0095f7`，kimi-k2.7-code，用户实时观察）。完整报告 `/opt/worker/report/prx-chat-demo-test-2026-07-06.md`，发现清单 `/opt/worker/task/prx/followup-chat-demo-findings-2026-07-06.md`。
**核心改造（parity + UX 第二轮）真机已全部工作**，本轮只做**体验打磨 + 用户新反馈**，不动已验证的渲染/工具卡/紧凑播报等主链路。

**规矩（同前，铁律）**：干净已提交状态自检；每 commit 前 + 每份 receipt 实跑 `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` 并写进 receipt；`cargo check --workspace --no-default-features`；全量 test。**禁止 push**。交互项改完标注"需 demo 真机复验"（主进程会在 tmux demo 驱动复验）。代码集中在 `src/chat/`（tui.rs / state.rs / renderer.rs / mod.rs / sessions/）。

---

## P1 — 用户直接反馈（高优先）

### U1：复制能力对标 Claude Code（F-U1）
**现象**：用户"无法复制"。`/copy` 命令层工作（"Copied latest assistant response to clipboard"），但：①依赖 OSC52，用户终端/tmux 链路未必通（实测 tmux `set-clipboard external`、`allow-passthrough off`）；②无鼠标拖选复制；③无法选择性复制某一段。
**目标**：
1. **确认并加固 OSC52 链路**：`/copy` 真把内容写进系统剪贴板；OSC52 序列正确（`\x1b]52;c;<base64>\x07`），并在 tmux/SSH 下尽量可达。线索：`src/chat/terminal_proto.rs`、`src/chat/mod.rs`（现 /copy 实现）。
2. **鼠标拖选复制**：全屏 alt-screen 默认吞鼠标（用于滚轮/会话导航）。参考 Claude Code：提供一种让用户能拖选复制的途径——如**按住某修饰键临时释放鼠标捕获**给终端原生选择，或提供"进入选择/复制模式"命令/快捷键。调研 CC 做法后择一实现，receipt 说明方案。
3. **可发现性**：footer 或 `/help` 明示复制方式（`/copy` + 拖选途径）。
**验收**：`/copy` 后剪贴板真有内容（尽力验证，注明所用终端）；拖选或复制模式可用；footer/help 有提示。demo 真机复验。

### U2：底部运行子进程条对标 Claude Code（F-U2，先调研+提案再实现）
**现象**：用户"底部运行的子进程、执行 shell 等，不像 Claude Code"。现状横排 strip `⏳/✓ #N shell Ns cmd…`（`+N›` 溢出）。
**要求**：**先调研 Claude Code 运行任务/子进程的呈现**（实时输出预览、spinner 动效、可展开查看、卡片/列表布局、进度），在 receipt 写出**差距清单 + 改造提案**，并实现其中明确的子项：
- 运行中会话给**实时输出末行预览**（而非仅 `cmd…` 命令回显）；
- 运行态 spinner 动效（对齐主生成的 `⠋` 风格）；
- 可展开查看某会话实时输出（若已有 `/logs #N`，考虑更顺手的展开入口）。
**注意**：此项用户描述较泛，**存疑处在 receipt 提问、别猜著大改**；主进程会拿提案找用户确认后再定最终形态。先实现无争议子项。
**验收**：strip 呈现更接近 CC；receipt 附差距清单+提案+已实现子项。demo 真机复验。

### U3：方向键导航对标 Claude Code（F-U3，需设计一致的键位模型）
**现象**：用户"Claude Code 可以直接用方向键上下左右调整"。现状 prx：裸 Up/Down = 输入历史，Alt+方向键 = 会话条导航（需修饰键）。
**要求**：调研 CC 裸方向键在各语境的语义，设计**一致且不冲突**的键位模型，让裸方向键尽量直达导航，同时**保留输入历史可达**。可行方向（供参考，codex 定稿）：**按上下文态**——输入框有内容时方向键管光标/历史，输入框空闲时裸 Up/Down 直接走会话/消息导航；或把会话导航从 Alt+箭头下放到裸箭头、历史改由其他键。**在 receipt 写出最终键位表**。footer/help 同步。
**验收**：裸方向键导航更像 CC 且不吞输入历史；receipt 附键位表；快照/单测覆盖关键分支。demo 真机复验。

---

## P2 — 打磨项（非阻塞，主进程发现）

### U4：`cost unknown` → 补 kimi 定价（F-1）
**现象**：`/cost` 与状态栏显示 `cost unknown`（token 记账本身正确：real 9.5k/est 0）。
**目标**：给 kimi-code / kimi-k2.7-code 补定价条目，使成本能算出。线索：`src/cost/types.rs`、`src/router/models.rs`、`src/config/schema.rs`（定价来源）。若定价应由用户配（cost.toml），则在无定价时状态栏显示更友好（如 `cost n/a`）并在 /cost 提示如何配。
**验收**：配了定价后 /cost 出真实金额；或无定价时文案友好且有引导。

### U5：slash 过滤只按命令名（F-2）
**现象**：`/mo` 除 `/model`/`/memory` 还带出 `/copy`/`/plan`/`/edit`/`/auto`（fuzzy 命中了描述里的 "mode"/"most"）。
**目标**：slash 菜单过滤**主要按命令名**匹配（描述可作次级/加权，但不该让纯描述命中挤进结果）。线索：`src/chat/tui.rs`/`state.rs` 的 slash 菜单过滤。加回归测试：`/mo` 结果只含命令名含 "mo" 的项。
**验收**：过滤精确；单测覆盖。

### U6：输入框"幽灵输入" + Esc 残留（F-3）
**现象**：①单次 Esc 关 slash 菜单后输入框残留 `/`（需再按一次才清）；②命令回车执行后，输入框仍以**亮色**显示上一条命令，像"幽灵输入"（就是测试中误认的 `/resume`/`/copy` 之源）。
**目标**：①Esc 关菜单时一并清掉触发字符（或明确双 Esc 语义并在 help 注明）；②执行后输入框回到干净空态；若是 reedline 历史 autosuggestion，应渲染为**明显暗色 hint** 而非亮色，避免误认。线索：`src/chat/tui.rs`（reedline 输入渲染/hinter 配置）。
**验收**：Esc 后无残留字符；执行后输入框空净或 hint 明显暗色可区分；快照测试。demo 真机复验。

### U7：Thinking 折叠块鼠标点击展开（F-4）
**现象**：`▸ Thinking (N tokens)` 默认折叠（用户认可），但无法展开看思考全文。
**目标**：**默认折叠保留**；让 `▸ Thinking` 行成为鼠标可点击热区（SGR mouse 已用于滚轮，扩展 click 命中检测到该行 → toggle 展开/收起，展开显示完整 reasoning，`▾` 收起）；可选加键盘展开键并在 footer/help 标注。线索：`src/chat/tui.rs`/`renderer.rs`（thinking 块渲染 + mouse 事件）、`src/chat/state.rs`（折叠态）。详见 `task/prx/followup-chat-thinking-block-mouse-expand.md`。
**验收**：点击 `▸ Thinking` 展开/收起；默认折叠；单测覆盖折叠态 toggle。demo 真机复验。

---

## 建议节奏
- P1 三项（U1/U2/U3）各含调研/设计成分，**每项独立 commit + receipt**，U2/U3 的设计提案先写进 receipt 好让主进程找用户确认。
- P2 四项（U4-U7）较机械，可合批。
- receipt 命名 `collab-outbox/receipt-2026-07-06-chat-ux-round3-<U?>.md`。禁止 push。
