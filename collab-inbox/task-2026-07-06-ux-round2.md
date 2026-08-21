# [派工] Post-parity UX 第二轮（用户已拍板）

基线：main HEAD `490fdd5d`（parity 战役 18 commit 之上）。规矩同前：**禁止 push**、receipt 自检在干净已提交状态跑、每 Batch 写 receipt 到 collab-outbox/、双路径 parity + plain no-render + GP-8 字面锚。背景报告：`/opt/worker/report/prx-chat-parity-audit-2026-07-05.md`（INPUT/SESS/TUI 条目）+ `/opt/worker/task/prx/followup-ollama-numctx-empty-turn.md`（O 项）。

## Batch A（先做）：provider 修复（O1+O2，各一 commit）

- **O1 [HIGH] ollama num_ctx**：`src/providers/ollama.rs` `build_chat_request` 的 `Options` 加 `num_ctx`。取值优先级：`[providers.ollama] num_ctx` 显式配置 > 该模型 RouterModelConfig.max_context（如已配）> 默认 8192。应用时 debug log 一条。背景：默认 4096 被 ~4.3k 系统注入吃满 → 首轮 prompt 截断、`content:""`（抓包实证 prompt_eval_count=4096）。测试：request 序列化含 num_ctx 三档取值。
- **O2 [MED] 空 assistant 轮**：流式收尾 content 为空（含只有 reasoning 无 content）时：①TUI/plain 发一条可见提示 "model returned empty response"（复用 system message 通道）；②**不持久化空 assistant turn**（history/session.turns 都不进）。legacy + Redux 两路径同改；注意别误伤"合法空回答后跟工具调用"的场景（tool_calls 非空时不算空轮）。测试：空 content 无 tool_calls → 不入库+有提示；有 tool_calls → 正常。

## Batch B：输入链路（I1/I2/I3，各一 commit）

- **I1 @path 补全菜单**（INPUT-3/ISS-020）：输入 `@`（词首）弹文件路径补全，复用 SlashMenu 骨架（`AtCursorContext` 对照 `slash_cursor_context`）。候选：相对 cwd 目录枚举，**必须走 file_read SecurityPolicy 白名单**（不得绕过）；子串/模糊过滤；`/` 结尾目录可下钻；Tab/Enter 补全路径进输入；Esc 收起；候选 cap ~50 + 排序（目录优先）。双路径 parity（reducer 同步 sources 模式对照 SlashMenuSources 的教训——**源经 Action 装填，别再出现 reducer 空源**）。plain 不渲染。
- **I2 粘贴折叠 chip + 输入区滚动**（INPUT-6）：①Paste 超阈值（>5 行或 >1KB）折叠为 `[Pasted text #N: M lines]` 占位 chip（buffer 保留原文，渲染显示占位，提交时展开原文；多个粘贴递增编号）；②输入区超过 INPUT_MAX_VISIBLE_ROWS(10) 时垂直滚动窗口跟随光标（修"粘贴长文光标消失" tui.rs:5107 区）。32KB 上限保留但截断时给可见提示（现在是静默）。
- **I3 输入行软换行**（INPUT-7）：`render_input` 加 wrap（Paragraph::wrap 或手工），光标按 wrap 后坐标定位（参考 transcript 的 unicode-width 光标定位既有代码）；与 I2 的滚动窗口协同（滚动单位=视觉行）。

## Batch C：会话与复制（S1+C1，各一 commit）

- **S1 idle 判据改 last-activity**（SESS-3）：`idle_warning_seqs`（runtime.rs:493 区）判据从 `now-started_at` 改为最后活动时间——agent/shell 用 SessionRing 最后 push 时间、PTY 用最后 read 时间；无输出满 10min 才标 ⚠ idle。测试：持续输出的长任务不标；停输出 10min 标。
- **C1 /copy 命令**（TUI-5 缓解）：`/copy` 复制最近一条 assistant 回答原文（markdown 原文非渲染后）到剪贴板，用 **OSC 52** 转义序列（tmux/SSH 下也通，无外部依赖；注意 base64 + 长度上限 ~74KB 截断提示）；`/copy N` 复制倒数第 N 条。plain 模式下直接打印原文（无 OSC）。注册进 COMMAND_SPECS（slash 菜单自动可见）。

## Batch D（追加，C 之后）：message_send 收件人竞态（HIGH，一个 commit）

- **背景**（审计报告 `report/prx-wa-im-chat-management-audit-2026-07-06.md` 缺口#1）：`message_send` 的 `default_recipient`/`active_channel` 是进程级全局，每条入站消息覆写（mod.rs:2628-2632 → src/tools/message_send.rs:205-211）；消息分发只对同会话串行，不同聊天窗口并行。并发 turn 下 LLM 无显式 target 调 message_send 会取到另一 turn 的 recipient → **回错窗口/群私串扰**。与已修的 sessions_spawn 跨渠道泄露（SPAWN_EXECUTION_CONTEXT task-local，mod.rs:3165-3187）同类。
- **修法**：照抄 SPAWN_EXECUTION_CONTEXT 模式——default recipient/channel 改 per-turn task-local，工具解析默认收件人时读 task-local；全局 `set_active_recipient` 广播删除（或仅留 CLI 单会话 fallback）。
- **测试**：并发双 turn 场景（A 窗口 turn 进行中，B 窗口消息到达覆写全局）下 message_send 无显式 target 仍发往 A 的 reply_target——修复前应红。注意非 turn 上下文（cron/system 触发）的 fallback 行为要保留且有测试。

## Receipt

每 Batch 一个：`collab-outbox/receipt-2026-07-06-ux-round2-batch<A|B|C|D>.md`。Batch 间不等审计可连做（审计并行跟进），但 fix-round 优先于下一 Batch。
