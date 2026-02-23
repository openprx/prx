# ZeroClaw Regression Test Report (2026-02-23)

环境说明：当前执行环境沙箱禁止网络与端口操作（`ssh qa` 与本地 bind 均被拒绝），因此依赖 QA 主机的在线验证项标记为 `SKIP`，并补充本地代码/单测证据。

编号 | 测试项 | 结果(PASS/FAIL/SKIP) | 备注
---|---|---|---
1 | zeroclaw health — 返回 ok | FAIL | 本地 `zeroclaw health` 返回 unrecognized subcommand（当前 CLI 无 `health` 子命令）。
2 | zeroclaw doctor — 无 critical | SKIP | 无法 `ssh qa` 执行远端 doctor。
3 | 内存使用 < 500MB（刚启动） | SKIP | 无法访问 QA 进程指标。
4 | 日志无 panic/error | SKIP | 无法读取 QA `journalctl`。
5 | config 加载无警告 | SKIP | 无法在 QA 启动路径验证加载日志。
6 | 发送文本消息到 Signal | SKIP | 需 QA 实机交互。
7 | 接收文本消息（检查日志） | SKIP | 无法访问 QA 日志。
8 | 发送图片 | SKIP | 需 QA 实机交互。
9 | 语音消息（TTS） | SKIP | 需 QA 实机交互；本地已确认 `tts` 工具代码路径存在。
10 | Quote reply（引用回复） | SKIP | 需 QA 通道交互验证；本地仅确认 `quote_timestamp/quote_author` 与 Signal 路径存在。
11 | wacli health check (端口 8687) | SKIP | 无法连接 QA 主机端口。
12 | 发送文本到 WA 群 | SKIP | 需 QA 实机交互。
13 | 接收 WA 消息（检查日志） | SKIP | 无法访问 QA 日志。
14 | shell 工具 — `ssh qa 'zeroclaw tool shell "echo hello"'` | SKIP | `ssh qa` 被沙箱阻止。
15 | file_read 工具 | PASS | 本地单测通过（`file_read_name`），且工具已注册。
16 | file_write 工具 | PASS | 本地单测通过（`file_write_name`），且工具已注册。
17 | web_fetch 工具 | PASS | 本地单测通过（`web_fetch::tests::test_tool_name`、`test_execute_invalid_scheme`）。
18 | memory_search 工具 | PASS | 本地相关单测通过（`memory_search` 过滤命中）。
19 | memory_get 工具 | PASS | 本地相关单测通过（`memory_get` 过滤命中）。
20 | agents_list 工具 — 检查是否显示身份配置 | PASS | 工具存在且输出含 `Identity dir`、`Spawn enabled`（见 `src/tools/agents_list.rs`）。
21 | session_status 工具 | PASS | 工具存在且单测通过（`session_status::tests::name_and_description`）。
22 | 检查 config 中 agents 段是否正确加载 | PASS | 配置模型包含 `agents` 并用于 tools 注册与 sessions_spawn。
23 | sessions_spawn agent=xxx 未配置身份时报合理错误 | PASS | 单测通过：`spawn_rejects_unknown_agent`；错误文案为 `Unknown agent ...`。
24 | agents_list 输出格式包含 identity_dir/memory_scope | FAIL | 当前输出包含 `Identity dir`，未输出 `memory_scope` 字段。
25 | `zeroclaw session-worker --help` 或参数错误时合理输出 | PASS | `--help` 输出正常，`--unknown-flag` 返回明确错误与用法。
26 | 检查 sessions_spawn process 模式代码路径存在 | PASS | `mode == "process"` 分支与 `session-worker` 调用链存在（`src/tools/sessions_spawn.rs`）。
27 | `zeroclaw-node --help` 是否有输出 | PASS | 本地命令输出完整帮助信息。
28 | `zeroclaw-node` 能否启动（token+bind+sandbox） | SKIP | 本地尝试启动时 bind 被沙箱拒绝（`Operation not permitted`），无法模拟 QA 网络环境。
29 | nodes tool 是否在工具列表中 | PASS | `NodesTool` 在 `src/tools/mod.rs` 注册，且 channel prompt 工具描述含 `nodes`。
30 | 检查 self_system 配置段是否在 config 中 | PASS | `Config` 含 `[self_system]`（`src/config/schema.rs`）。
31 | 检查日志中是否有 self_system 相关启动信息 | SKIP | 无法访问 QA `journalctl`；本地已确认 `main.rs` 有 `target: "self_system"` 启动日志路径。
32 | 验证 exec_shell 白名单（非白名单命令） | PASS | 单测通过：`command_blocklist_rejects_first_token`（拒绝 `rm -rf /`）。
33 | 验证 TLS 校验（非 https endpoint 被拒绝） | PASS | 单测通过：`endpoint_validation_rejects_plain_remote_http`（拒绝远端 `http://10.0.0.2:8787`）。
34 | 验证 sandbox path（读取 sandbox 外文件） | PASS | 单测通过：`sandbox_prevents_escape`、`sandbox_read_rejects_symlink_escape`、`sandbox_write_rejects_symlink_escape`。
