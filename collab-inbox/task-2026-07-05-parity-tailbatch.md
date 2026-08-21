# [派工] Parity 战役尾批（小项收口，一个 commit 即可）

前置：P0+P1 全部 ACCEPT（含 3a fix-round 四 commit 与 3c `09f8c631`）。本尾批是战役最后一批代码改动，之后进入终验。仍禁止 push。自检按规范在干净已提交状态跑。

## 必做

1. **inline 强调 flank guard**（3c 审计唯一实质 CONCERN）：`render_inline_markdown` 的 `*`/`_` 斜体解析加约束——emphasis body 首尾必须非空白（挡掉 `width * height * depth` 把 " height " 误斜体、列表行 `* item` 误触发）；`_` 额外要求词边界（挡掉 `snake_case_name` 把 "case" 误斜体）。补这三个字面场景的负向测试（渲染结果不含 `\x1b[3m`）。
2. **F9 测试补牙**：旧 JSON（缺 cache_creation/cache_read 字段的 MeteredTokenUsageRecord/TokenUsage）反序列化回归测试，锚 cache 字段为 None/0 且其余字段正常。
3. **F10 测试补牙**：①sessions_tick reap 清 strip_selection 的测试（可测 helper 层：entries 不含选中 seq → 清除+dispatch）；②reducer 侧无选中 Alt+Enter fall-through 插换行测试（对齐 mirror 侧已有）。
4. **"nothing to drop" 去重**：Redux preflight 卡超限（over_hard_limit 且无可压）时每 turn 重发一条 feedback——加去重（同状态只发一次，状态变化后可再发）。
5. **F8 SHOULD 收尾**：①feedback 发射测试（Redux preflight/overflow 的 SystemMessageAdded + legacy preflight 块）；②Redux 驱动 mid-turn 压缩后补发 ContextWindowUpdated（对齐 legacy 立即回落）；③低于触发阈值的 `/compact` fallback 改 noop（防连续 /compact 把摘要 char-trim 截毁），feedback 报 "nothing to compact"；④`ui_dirty_for` 的 HistoryCompactionPatchApplied 过时注释修正。

## 明确 backlog（不做，记录在此供后续）

providers env 锁/landlock pid 竞争根治；F4 渲染缓存 seq 键控重设计（512 缓解在位）；transcript view 滚动 clone → Arc/offset；footer 发现性；keyboard/markdown 全局态测试窄 flake 窗口；F12 内容级锚（resize 漂移）；F6 error 首行重复展示注明；reducer picker Enter-resume parity（driver 侧 effect 范围）。

## Receipt

`collab-outbox/receipt-2026-07-05-parity-tailbatch.md`：commit、测试名、干净状态自检、二进制路径。
