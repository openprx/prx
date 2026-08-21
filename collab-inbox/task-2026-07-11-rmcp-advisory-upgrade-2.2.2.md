# Task 2026-07-11 — 根治 rmcp advisory RUSTSEC-2026-0189（升级到 2.2.2）

**基线**：分支 `fix/rmcp-advisory-rustsec-2026-0189`（off main `fd455744` v0.8.0，工作树干净）。**别 git commit**（提交我来做）。全程 `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp`。别自提交、别碰无关代码。

## 目标
把 rmcp 从 0.14.0 升到 **2.2.2**（用户指定最新 2.x；若 crates.io 无 2.2.2 则用最新可用 2.x 版本并在 receipt 注明确切版本），同时删除未使用的 `server` feature，摘掉 advisory ignore，令 `cargo audit` + `cargo deny check advisories` 不再命中 RUSTSEC-2026-0189，且编译/功能不破。

## 背景（审计已确认，供你参考）
- 全仓库唯一引用 rmcp 的文件是 `src/tools/mcp.rs:9-10`（`use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess}; use rmcp::{ServiceExt, model::CallToolRequestParams};`），纯 **client** 用法（`().serve(transport)` + `list_all_tools`/`call_tool`/`cancel`），无任何 `ServerHandler`/server。删 `server` feature 不影响编译（client feature 单独即可导出 `ServiceExt`/`Service`/`Peer`）。
- advisory 修复版 `>=1.4.0`；升 2.2.2 必然摘掉命中。
- ⚠️ **2.0.0 有两处 model 类型 BREAKING**，升 2.x 大概率要改 `mcp.rs`：
  - #919 "relax tool result structuredContent type"
  - #927 "align model types with MCP 2025-11-25 spec"
  - 重点复核 `extract_call_success_and_output`（`src/tools/mcp.rs:401-437`）对返回 `content`/`isError`/`structuredContent` 的手工 JSON 解析假设，以及 `CallToolRequestParams` 的字段（`task`/`meta` 等）。逐个按 `cargo check` 报错修，改动应局限在 mcp.rs 单文件。

## 改动清单
1. `Cargo.toml:121`：
   - 版本 `"0.14"` → `"2.2.2"`（或 `"=2.2.2"` 精确锁；若无此版本用最新 2.x 并注明）。
   - features 删 `"server"`：结果 `["client", "transport-child-process", "transport-streamable-http-client", "transport-streamable-http-client-reqwest"]`。确认这些 feature 名在 2.x 仍存在（2.x 可能重命名了 transport feature——若报错，按 2.x 的 feature 名调整并在 receipt 说明改了哪些）。
2. `cargo update -p rmcp` 刷新 `Cargo.lock` 到 2.2.2。
3. `src/tools/mcp.rs`：按 `cargo check` 编译错误逐个修 2.x breaking（client 侧 API + JSON 解析）。**只改让它编译+语义等价所必需的，别顺手重构无关代码**。每处非平凡改动在 receipt 里说明"2.x 什么变了、你怎么改的、为什么语义等价"。
4. `.cargo/audit.toml`：删除 RUSTSEC-2026-0189 的 ignore 条目（约 16-20 行带注释的块）。
5. `deny.toml:25`：删除对应 `RUSTSEC-2026-0189` 行。

## 验收（receipt 写 `collab-outbox/receipt-2026-07-11-rmcp-advisory-upgrade.md`，别 commit）
逐条实跑贴结果：
1. rmcp 锁定版本确认：`cargo tree -i rmcp`（贴版本 = 2.2.2）。
2. `cargo fmt --check`
3. `cargo clippy -p openprx --all-targets --all-features -- -D warnings`
4. `cargo check -p openprx --all-features` + `--no-default-features`
5. 全量 `cargo test -p openprx --bin prx --all-features` —— 报 `N passed; M failed`；**重点确认 mcp.rs 的测试全过**（SSRF/SideEffectGate/validate_and_call/extract_call_success_and_output 相关用例），逐个列出 mcp 相关测试名 + 结果。
6. **`cargo audit`**（贴 summary，确认**不再**出现 RUSTSEC-2026-0189）+ **`cargo deny check advisories`**（贴 `advisories ok`）。因为 ignore 已删，若此时仍命中说明版本没升上去，需排查。
7. **E2E（铁律，真机 MCP client 功能不破）**：配一个本地 stdio MCP server（如现成的 mcp echo/filesystem server，或用 npx `@modelcontextprotocol/server-*`）+ 若可行再配一个 streamable-http MCP server，跑 `prx`（或最小复现）实际发起 MCP 工具发现 + 调用，确认 rmcp 2.2.2 下 `discover_server_tools_stdio`/`_http` 握手、`list_all_tools`、`call_tool` 返回解析正常。贴关键输出。**若环境无法起真 MCP server**，明确说清卡点，退而用 mcp.rs 的集成测试覆盖证明 client 链路，别假装跑了。
8. 明确写：**未 commit**、rmcp 确切版本、mcp.rs 改了哪些 2.x breaking、audit/deny 是否绿、E2E 结果。

铁律：零 unwrap/expect（生产码）、零 warning、English 代码注释与 commit 文案。别自 commit。
