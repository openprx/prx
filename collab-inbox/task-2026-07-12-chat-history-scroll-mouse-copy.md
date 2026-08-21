# Task 2026-07-12 — prx chat 历史查看 / 鼠标复制（A+B+C 一单）

**基线**：分支 `feat/chat-history-copy-guidance`（off `feat/chat-file-edit-diff-rendering` tip `c998ad79` v0.8.4，工作树干净）。**别 git commit / 别自提交 / 别 bump 版本**（提交+版本我来做）。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。

用户反馈：`prx chat` ①无法展示历史记录 ②鼠标点击某行想复制不支持。只读审计（`task/prx/audit-chat-history-scroll-mouse-copy-2026-07-11.md`）已定三真因，用户决策 = **A+B+C 全做，/copy 不扩展**。行号以磁盘为准（审计基线 main，本分支已叠 diff 工作，落手前先 grep 确认锚点）。

关键背景（必读，别推翻）：
- 鼠标捕获**默认 OFF**（`mouse_capture_enabled_by_env()` mod.rs:549，仅 `PRX_TUI_ENABLE_MOUSE=1` 才开）——这是**有意设计**：默认让出鼠标给终端原生拖选复制。**别改默认**。
- app 内历史**无截断**、能滚到最开头（Home/PageUp/Ctrl+O；`conversation_lines` 仅 push 无 cap）。用户"看不到历史"真因 = **可发现性**（不知道这些键 + alt-screen 杀了终端原生 scrollback），不是数据丢失。
- `prx chat --plain` / `PRX_TUI=0` 已是现成兼容出口（不进 alt-screen、不开鼠标、不 raw mode → 终端原生 scrollback + 跨历史拖选复制全恢复），缺的只是运行时引导。

---

## B — 必修 bug（最高优先，确定性回归）

`src/chat/sessions/pty.rs:294`：PTY 子程序（vim/less/htop）detach 时**无条件 `EnableMouseCapture`、无视 env 门控**。后果 = 挂过一次全屏子程序后，本会话鼠标捕获被强开，默认的终端原生拖选复制失效（"中途鼠标复制突然失效"）。

- 改为**遵守 env 门控**：detach 重开鼠标前，判断本会话鼠标捕获是否本应开启（即 `mouse_capture_enabled_by_env()` 为 true / 会话启动时 `mouse_capture_active` 为 true）。为 false → **不要重开**。
- 该重置块（pty.rs:285-296）还无条件重开 raw_mode / bracketed_paste——这些是 chat 基线，保留；**仅** mouse capture 这一行加门控。
- 需要把"本会话是否启用鼠标"的状态传进 `PtyHandoffGuard`（它已持有 `keyboard_enhancement_active`，仿此加一个字段，从建卡处传入）。别用全局可变；顺现有 struct 字段传递。
- 更新块内注释（1b 那段）说明鼠标重开现在受门控，别留误导。

## A — 补引导（可发现性，收敛到现有渲染路径）

在下列出口明确告诉用户"历史/复制怎么用"，文案**别与现有 footer 打架**，走现有 footer 渲染路径（`src/chat/tui.rs:7140-7154`）：
- **看全部历史** = `Home` / `PageUp` / `Ctrl+O`（**不是**终端滚动条——alt-screen 下终端滚不动）。
- **要终端原生滚动 + 跨历史框选复制** = 用 `prx chat --plain`（或 `PRX_TUI=0`）重启。
- **复制/导出** = `/copy latest|N`（OSC52 复制 assistant 段，mod.rs:496-540）+ `/export md|json`（导出整份，commands.rs:288/556）。

落点（按收益，别全塞 footer 挤爆）：
1. footer 提示补上"Home/PageUp 看历史"+"--plain 可拖选复制"的极短提示（现有 footer 已提 drag select，措辞对齐）。
2. `/help` 里加一节"历史查看 & 复制"完整说明（这里放长文案）。
3. 空 transcript 占位 / 首屏提示带一句"按 PageUp 看历史，/help 查复制"。
- 文案中英按现有 chat UI 语言惯例（跟随现有 footer/help 语言）。

## C — 切换键释放鼠标捕获（只服务设了 `PRX_TUI_ENABLE_MOUSE=1` 的用户）

给开了鼠标捕获（要滚轮/点击展开）的用户一个**切换键**，一键临时进入"选择模式"让出鼠标给终端原生拖选，再按恢复。
- **键位**：选一个**不冲突**的键。**先读 `task/prx/design-chat-key-model-rearrange-2026-07-11.md` + 全量核对现有 KeyDispatch 键表**，确认不撞任何现有绑定再定（候选如 `Ctrl+Space`，但以实测不冲突为准；receipt 里写明最终选键 + 你怎么确认没冲突）。
- **行为**：按下 → `disable_mouse_capture()`（mod.rs:2930）进选择模式，状态栏/footer 提示"选择模式：拖选复制，再按 <键> 恢复"；再按 → `enable_mouse_capture()` 恢复。选择模式内滚轮滚动 / 点击展开 thinking 卡**暂失效可接受**。
- **门控**：仅当鼠标捕获当前为 on（`mouse_capture_active`）才有意义；默认 OFF 的用户按了应无害（no-op 或直接提示"鼠标未启用，默认即可拖选复制"）。
- 改点：mod.rs:12401 附近事件处理 + mod.rs:2884-2930 终端 ops（复用现成 enable/disable）+ 新增 KeyDispatch 分支 + footer/状态栏提示状态。
- **默认鼠标仍 OFF**（维持，利于复制）。

## 不做（用户已定）
- D（app 内键盘选行复制）：`--plain` 已覆盖，不划算。
- `/copy` 扩展到任意行 / 用户消息 / 工具输出：暂不做（任意行走 `--plain` 原生拖选）。

---

## 验收（receipt 写 `collab-outbox/receipt-2026-07-12-chat-history-mouse-copy.md`，别 commit）
逐条实跑贴结果：
1. fmt `--check` / clippy `--all-targets --all-features -D warnings` / check（双 feature）。
2. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 passed/failed 总数；新增/改测试列名（B 门控：mouse-off 时 detach 不重开鼠标的单测；C 切换键 toggle 单测；A footer/help 文案渲染测试）+ 关键 mutation 证牙。**别只跑过滤子集**。
3. `cargo audit` + `cargo deny check advisories`（含 bans/licenses/sources）双绿；确认无 RUSTSEC-2026-0189 回归；本单预计无新依赖，若引入需在 receipt 说明并过 deny。
4. **真机 PTY demo（关键，逐个贴 tmux 截录）**：
   - **B**：`PRX_TUI_ENABLE_MOUSE` 不设（默认 OFF）启动 chat → 在内挂一次全屏子程序（如 `vim`）→ `Ctrl+]` detach 回 chat → 确认鼠标捕获**未被强开**（终端原生拖选仍可用 / 无鼠标转义乱码）。对照修复前是坏的。
   - **A**：截 footer 显示新引导；`/help` 截"历史查看&复制"节；空/首屏占位提示截图。
   - **C**：设 `PRX_TUI_ENABLE_MOUSE=1` 启动 → 按切换键 → 状态栏显示"选择模式" + 鼠标让出（拖选可用）→ 再按恢复（滚轮/点击展开回来）。贴前后截录 + 写明最终选键。
   - 跑不动说清卡点，别假装。
5. 明确写：**未 commit**、A/B/C 各改动文件:行、C 最终选键 + 无冲突依据、是否引入依赖、真机 demo 观感。

铁律：零 unwrap/expect（生产码）、零 warning、零死代码、English 代码/commit/注释、output/预览别无界、Mutex 用 parking_lot/tokio。别自 commit、别 bump 版本。
