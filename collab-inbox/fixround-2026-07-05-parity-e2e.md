# [派工] 真机 E2E 终验发现修复（战役收口最后一批）

真机 E2E 总验证 9/10 PASS，发现 2 个需修项。在 no-default-features 修复之后做。仍禁止 push。

## 必修（一个 commit）

1. **[HIGH] Ctrl+O transcript 子会话恒为空（单源渲染违例）**：`open_transcript_view`（mod.rs:7559 区）从 TuiState mirror 读 `guard.conversation_lines`，但实时对话在 Redux `state.ui.conversation_lines`（渲染侧 tui.rs:10788 区用的就是 Redux 源）；mirror 该字段只在 resume（mod.rs:9176 区）等少数点填充 → 7+ turns 含工具输出时 Ctrl+O 仍显 "(transcript is empty)"，F6 工具卡 "Ctrl+O for full transcript" 承诺实机不成立。修法：transcript 构建改从 Redux snapshot 的 conversation_lines 取（与渲染同源），mirror 路径仅作 fallback。**测试要求**：现有 transcript 测试之所以没抓住，是因为直接预填了 mirror——新测试必须走"对话只存在于 Redux 侧"的真实状态形态，修复前应红。
2. **[LOW] attach shell 子会话 prompt 误显 `agent #1 ▸`**：应按 ManagedKind 显示 `shell #1 ▸`。

## 记录（不修，已另立任务待用户决策）

- ollama provider 不传 num_ctx（默认 4096 被系统注入吃满 → 空回答）——provider 范围，任务文件 task/prx/followup-ollama-numctx-empty-turn.md。
- 空 assistant 轮静默持久化——同上任务文件。
- attach 态 detach 需两次 Esc（第一次剥 strip 选中层）——与 overlay 分层自洽，等用户确认是否 intended。

## Receipt

`collab-outbox/receipt-2026-07-05-parity-e2e.md`：commit、"修复前会红"测试说明、干净状态自检。
