# Task: Fix 12 pre-existing full-suite test failures (blocking P4)

- **Date**: 2026-07-09
- **Status**: FINAL(Claude 分诊 + 定稿)
- **Base**: `cbde09ce`(P3c)。P4a 已 stash(`P4a-wip-needs-fixround-A1-E1-A2`),本批在干净 P3c 上做。
- **Owner(实现)**: Codex / **Audit**: Claude 子进程 + **全量 `cargo test -p openprx --bin prx --all-features`**
- **背景**:全量测试(过滤子集跑不出)暴露 12 个 FAILED。分诊确认**全部真 bug、零环境敏感**。11 个由 substrate 基线 `75c7823d` 引入(Claude Code parity 键路由回归),1 个由 P2 引入(worker 视图 io 设计变更)。契约:**裸箭头=输入历史/子视图滚动,ALT=strip/会话导航**(Claude Code parity)。

## ⚠️ 修前必做
在改 `dispatch_global_key` 裸箭头路由前,**grep 全部当前通过的测试**,确认没有测试依赖"裸箭头导航 strip/会话"的旧行为(契约冲突)。从失败测试命名看契约站在"裸箭头=历史/滚动"一边;若发现冲突的通过测试,停下报告,不要盲改。

## 代码修复(8 个代码错)

### 修复 1 — 恢复 marker unicode 分支(清 #3 #4)
`tui.rs:4411` `session_active_marker`:`75c7823d` 把它改成永远返回 `>`、忽略 `_ascii`。恢复:`if active { if ascii { ">" } else { "\u{25B8}" } } else { " " }`(宽度均为 1,零风险)。
- #3 `sessions_strip_one_active_entry_shows_marker_glyph_kind_and_title`
- #4 `sessions_strip_active_entry_drives_window_when_no_selection`

### 修复 2 — 裸箭头还给 input-history/滚动(清 #5 #6 #7 #8 #9)
`tui.rs:1557-1578` 块:Main+空输入+**裸**箭头 → strip 选择/Consumed,抢走裸 Up/Down。移除该块里**裸箭头方向**处理,让裸箭头落到 input-history 召回/光标;**保留** 1579-1598 的裸 Enter-attach。ALT 版(块 1600)已独立承担 strip 选择,不动。
- #5 `input_history_up_down_still_work_when_slash_menu_closed`
- #6 `input_history_edit_then_up_down_steps_clean_entries`
- #7 `saved_session_picker_closed_keeps_up_down_input_history_behavior`
- #8 `fullscreen_scroll_focus_rules_do_not_steal_input_or_child_keys`
- #9 `bare_arrows_history_cursor_and_child_scroll_are_not_stolen_by_strip_selection`

### 修复 3 — session 焦点裸箭头还给子视图滚动(清 #10)
`tui.rs:1674-1707` 块:session 焦点+空输入+裸箭头 → 切会话,抢走本应给子视图滚动(块 1740)的 Up/Down。移除/门控该块裸箭头方向处理。
- #10 `dispatch_child_view_scroll_keys_only_when_child_focus_and_empty_input`

### 修复 4 — main 合成条目排除出 ALT strip 环绕(清 #11)
`#11 alt_arrows_move_ui_only_strip_selection_without_focus_change` 期望 Alt+Up 从 1 环绕到 3(main 不参与)。`bottom_chrome_session_entries_with_workers` 把合成 main 条目(MAIN_SESSION_SELECTION_SEQ=0)塞进 ALT strip 环绕列表。按 parity 契约:**ALT strip 环绕应只在真实 session/worker 间,排除合成 main 条目**。据此改代码使环绕跳过 main。
- #11(若你核查后认为 main 该参与环绕,停下报告,与产品对齐再改测试)

## 测试更新(代码是对的/刻意设计)

### 修复 5 — args_preview 期望(清 #1)
`state.rs:8754`:代码 `build_tool_args_preview`→`format_shell_preview`(tui.rs:6366)刻意把 shell 参数美化为 `command="ls"`,姊妹测试 tui.rs:7746 已锁定同格式。**改测试**期望为 `command="ls"`。
- #1 `s2_5_p1_b_assistant_turn_carries_tool_calls`

### 修复 6 — worker 视图非流式态 io 期望(清 #2)
`#2 provider_worker_status_update_refreshes_open_worker_view_with_io`:P2 刻意设计 `provider_worker_io_lines_for_streaming_draft` 在 `streaming==None` 时返回空(io 只从匹配 streaming draft 来,避免 P3 前无 task 归属的历史串流)。测试塞了已完成 ToolResult 无 streaming draft → 期望 io 行,与 P2 设计冲突。**改测试**:期望非流式态 worker 视图 io 为空(对齐 `phase2_provider_worker_io_none_is_empty_not_history_fallback` 已锁定的设计)。
- #2

### 修复 7 — footer 文案期望(清 #12)
`#12 fullscreen_footer_hides_completed_sessions_from_active_bottom_list`:前半(隐藏 completed)已过;footer 不再含字面 `Ctrl+G sessions`(仓内 grep 无此串)。核对 footer 现文案:若为刻意重设计 → **改测试**对齐现文案;若提示被误删 → 恢复提示(改代码)。给出你的判断依据。
- #12

## 验收门(Claude 审计 + 全量)
- **全量 `cargo test -p openprx --bin prx --all-features` 必须 0 failed**(这是本批核心门,不是过滤子集)。
- fmt/clippy(-D warnings)/check --all-features/check -p openprx --no-default-features 全绿零 warning;七铁律。
- 不得为了让 12 个过而弄坏其他当前通过的测试(净失败数必须归 0,不是"改了数字")。
- receipt 交 `collab-outbox/receipt-2026-07-09-fix-12-test-failures.md`,列每个测试的根因判定(代码错/测试过时)、改了代码还是测试、契约冲突核查结果、全量测试 before/after 失败数。
