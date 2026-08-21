# Task: Phase 4a fixround (event pump 骨架 N=1 等价性修复)

- **Date**: 2026-07-09
- **Status**: FINAL(Claude 审计发现,已定稿)
- **Base**: P4a 未提交改动现叠在 `68392f11`(12-fix 后的干净基线,全量 0 failed)之上,工作树含 src/chat/mod.rs、src/channels/terminal.rs、tests/chat_pty_e2e.rs。
- **Owner**: Codex / **Audit**: Claude 子进程 + **全量 cargo test --bin prx**
- **背景**:P4a(把 Redux turn 收口从内层 await 上提到外层事件泵 finalizer,N=1)经 Claude 审计判【需修复】。核心命题是 **N=1 严格等价**,审计发现 3 处破坏等价的问题 + 测试不足。本批修掉它们,不改并发(admission 仍 ==0,P4b 才放开)。

## 必修

### A1 — in-turn 本地命令输出 emit 句柄错配(必修)
`mod.rs:~4147-4169`(外层 select 处理 turn 运行期的 in-turn 输入):in-turn 本地命令(/queue、/workers 等)输出改用了 `surface_session_message(&chat_dispatcher, sessions_redraw_handle.as_ref(), text)`,而 `sessions_redraw_handle` 全程恒为 `None`(~4067 硬编码)→ 走 `print_fallback_chat_output` 原始 `print!` 直写 stdout,在 raw-mode TUI 下污染 ratatui 画面,违反单源渲染铁律。
- **旧行为(基线内层 msg arm)**:走闭包 emit,redraw 句柄是 `redraw_tx_for_main`(交互 TUI 下 Some → dispatch `SystemMessageAdded` + 触发重绘,不写 stdout)。
- **修**:该处 emit 改用 `redraw_tx_for_main.as_ref()`(此作用域可见),恢复 reducer 单源重绘。
- 加测试:in-turn 本地命令输出经 reducer(SystemMessageAdded)而非 raw stdout。

## 必须验证(查实后:真回归则修,不可达则注释说明并加防御)

### E1 — control_rx(ResumeSavedSession)在 pending turn 期间交织(潜在状态损坏)
基线中 detached turn 走内层 await,其内联 select **不含 control_rx**,故 turn 运行期 `ResumeSavedSession` 被推迟到 turn 完成后。P4a 把 turn 交回外层 loop 后,外层 select 的 control arm(~4278)会在 pending turn 存在时照常处理 `ResumeSavedSession` → `resume_saved_session_by_id` 就地替换 `chat_session`/`chat_session_key`/`history`(~4291)。而挂起的 finalizer 持开始时捕获的 `history_len_before_user_turn`,收口时 `history.push(assistant)` 或 `rollback_cancelled_turn_history(history, len)` 会写进/截断**被换掉的会话** → assistant 落错会话 / rollback 越界。
- **查实**:交互 TUI 下,turn 运行期间 control_tx 是否可能投递 ResumeSavedSession?(control_rx 在该模式接线吗?UI 是否允许 turn 中触发 resume?)
- 真可达 → 修:pending turn 存在时,control arm 的 ResumeSavedSession 要么延迟到 finalize 后,要么拒绝并提示,不得就地换会话。
- 不可达 → 在 control arm 加注释说明为何安全 + 一个防御性 guard(pending 非空时不处理 resume),并加测试锁定。

### A2 — draft 收口从 awaited 弱化为非阻塞(背压丢字)
非空 Completed 的 draft 最终化,旧内联用 `terminal.finalize_draft(await)`(等 ui_tx 容量),P4a 提取版改用新增 `terminal.try_finalize_draft`(非阻塞 try_send,满则丢;plain 分支 fallback print!)。主交互路径 task_id==Some 现恒走提取版 → draft 投递从"awaited 保证送达"弱化为 best-effort,背压下最终文本可能被静默丢弃且无 warn。
- **查实**:ui_tx 深度 128,交互路径是否存在背压场景(慢消费者 / 大量 turn)?
- 若可能丢字 → 恢复 awaited 语义(collection 收口处 await finalize_draft),或至少满时 `tracing::warn!` 不静默。
- 若确认无背压(容量充足、消费恒快)→ 注释说明 + 保留但加 warn。

## 补测(2 个测试不足以背书 914 行主循环重构)
按测试铁律补 PTY/单测覆盖以下等价性场景(审计 D10 列出):
1. `finalize_pending_redux_turn` 三分支端到端业务效果(history push / add_user+assistant_turn / usage record / draft 收口 / elapsed)。
2. `finalize_ready` 从 pending+completion 到实际收口整链。
3. shutdown 期间 `finalize_all_as_cancelled` 的 rollback + draft 清理。
4. control(ResumeSavedSession)在 pending turn 期间(E1 场景)。
5. input_events_open 关闭后仍等 completion 的收尾/退出。
6. /quit 在 turn 运行期间的入队-延迟退出流。

## 验收门(Claude 审计 + 全量)
- **全量 `cargo test -p openprx --bin prx --all-features` 必须 0 failed**(核心门,不是过滤子集!基线 68392f11 已是 0 failed,不得让它回退)。
- fmt/clippy(-D warnings)/check --all-features/check -p openprx --no-default-features 全绿零 warning;七铁律(零死码注意 crate allow)。
- admission 仍 active_workers==0(不放开并发,P4b 才做);主 transcript primary 不回归。
- receipt 写 `collab-outbox/receipt-2026-07-09-visible-turns-phase4a-fixround.md`:A1 怎么修、E1/A2 查实结论(可达性证据)+ 处置、补的测试断言、全量 before/after。
