# Receipt 2026-07-11 — rmcp advisory upgrade

## Scope
- Branch: `fix/rmcp-advisory-rustsec-2026-0189`
- Baseline requested: `fd455744` / v0.8.0
- Commit: not created.
- Changed repo files:
  - `Cargo.toml`
  - `Cargo.lock`
  - `src/tools/mcp.rs`
  - `.cargo/audit.toml`
  - `deny.toml`

## Version Decision
- Requested target: `rmcp 2.2.2`.
- Registry result: `cargo info rmcp@2.2.2` returned `could not find rmcp@2.2.2 in registry`.
- Installed/locked version: `rmcp 2.2.0`.
- `cargo info rmcp` reports `version: 2.2.0` and `crates.io: https://crates.io/crates/rmcp/2.2.0`.
- Important feature finding: `rmcp 2.2.0` default features are `[base64, macros, server]`, so simply deleting `"server"` from the feature list is insufficient. The dependency now uses `default-features = false`.

## Code Changes
- `Cargo.toml:121`
  - Changed `rmcp` from `0.14` to `2.2.0`.
  - Removed the explicit `server` feature.
  - Added `default-features = false` so rmcp's default `server` feature is not pulled back in.
  - Active features are now `client`, `transport-child-process`, `transport-streamable-http-client`, and `transport-streamable-http-client-reqwest`.
- `Cargo.lock:6131`
  - Locked `rmcp v2.2.0`.
  - `rmcp` now depends on `reqwest 0.13.4`.
  - Updated `sse-stream` to `0.2.4`; `rmcp 2.2.0` did not compile against the previously locked `sse-stream 0.2.1` because the old API lacked `SseStream::from_bytes_stream`.
- `src/tools/mcp.rs:439`
  - Added `call_tool_params`.
  - Reason: rmcp 2.x made `CallToolRequestParams` non-exhaustive / constructor-based, so the previous struct literal with `meta`, `name`, `arguments`, and `task` no longer compiles.
  - Equivalent behavior: `CallToolRequestParams::new(tool_name)` plus `.with_arguments(arguments)` sends the same tool name and optional argument object.
- `src/tools/mcp.rs:575` and `src/tools/mcp.rs:636`
  - Replaced both stdio and HTTP `client.call_tool(CallToolRequestParams { ... })` call sites with `client.call_tool(Self::call_tool_params(...))`.
- `src/tools/mcp.rs:401`
  - `extract_call_success_and_output` was reviewed against rmcp 2.x model serialization and existing tests. No production change was required: it still accepts `isError` and `is_error`, extracts text content, falls back to JSON for non-text/empty content, and preserves existing behavior.
- `.cargo/audit.toml:1`
  - Removed the `RUSTSEC-2026-0189` ignore entry and its explanatory block.
- `deny.toml:17`
  - Removed the `RUSTSEC-2026-0189` ignore entry.

## Feature / Advisory Checks
- `cargo tree -i rmcp`
  - `rmcp v2.2.0`
  - `└── openprx v0.8.0 (/opt/worker/code/prx)`
- `cargo tree -e features -i rmcp`
  - Shows only client/transport-related rmcp features.
  - No `rmcp feature "server"` entry is present.
- `rg RUSTSEC-2026-0189 Cargo.toml .cargo/audit.toml deny.toml Cargo.lock`
  - No match after the change.

## MCP Tests
- Targeted extract parser test:
  - Command: `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features extract_call -- --nocapture`
  - Result: `5 passed; 0 failed; 5480 filtered out`
  - Passed tests:
    - `tools::mcp::tests::extract_call_success_text_content`
    - `tools::mcp::tests::extract_call_error_flag`
    - `tools::mcp::tests::extract_call_is_error_snake_case`
    - `tools::mcp::tests::extract_call_empty_content_falls_back_to_json`
    - `tools::mcp::tests::extract_call_multiple_text_items_joined`
- Full test suite also covered the existing MCP gate/security tests, including:
  - `call_denied_without_grant`
  - `call_allowed_through_gate_with_grant`
  - `call_denied_with_mismatched_grant`
  - `call_allowed_with_matching_grant`
  - `readonly_blocks_execute`
  - `mcp_disabled_returns_error`
  - `load_config_blocks_malicious_server`
  - `load_config_blocks_ssrf_http_server`
  - `validate_http_url_blocks_localhost`
  - `validate_http_url_blocks_private_ipv4`
  - `validate_http_url_blocks_ipv6_loopback`
  - `validate_http_url_allows_public`
  - `test_same_name_server_command_override_blocked`
  - `test_same_name_server_matching_command_kept`

## True MCP Client E2E
- Built a temporary local stdio MCP echo server at `/opt/worker/tmp/prx-mcp-e2e/echo_mcp_server.py`.
- Important protocol note: rmcp 2.2.0 stdio transport uses JSON-lines framing, not Content-Length framing. The temporary server was adjusted accordingly.
- Ran a temporary binary outside the repo at `/opt/worker/tmp/prx-mcp-e2e/e2e_crate` that depends on `openprx = { path = "/opt/worker/code/prx", features = ["test-mock"] }`.
- The E2E constructs and uses the real PRX `McpTool`, not a standalone rmcp-only sample:
  - `McpTool::refresh()` -> `TokioChildProcess` -> `().serve(transport)` -> `list_all_tools`
  - `McpTool::execute_named("mcp__echo__echo", {"text":"rmcp-2x-e2e"})` -> `call_tool`
  - `extract_call_success_and_output` parses the rmcp 2.x `CallToolResult`.
- E2E command result:
  - `status=0`
  - stdout:
    - `discovered={"echo": [("echo", Some("Echo input text"))]}`
    - `success=true output=echo:rmcp-2x-e2e error=None`
  - server log:
    - `initialize 2025-11-25`
    - `initialized`
    - `tools/list`
    - `initialize 2025-11-25`
    - `initialized`
    - `tools/call echo text=rmcp-2x-e2e`

## Gates
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo fmt --check`
  - Passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo clippy -p openprx --all-targets --all-features -- -D warnings`
  - Passed: `Finished dev profile ... in 4m 34s`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --all-features`
  - Passed.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo check -p openprx --no-default-features`
  - Passed: `Finished dev profile ... in 1m 21s`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo test -p openprx --bin prx --all-features`
  - Passed: `5478 passed; 0 failed; 7 ignored; 0 measured; 0 filtered out`
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo audit`
  - Passed exit 0.
  - Scanned `914 crate dependencies`.
  - `RUSTSEC-2026-0189` no longer appears.
- `CARGO_TARGET_DIR=/opt/worker/tmp/prx-target TMPDIR=/opt/worker/tmp cargo deny check advisories`
  - Passed: `advisories ok`

## Final State
- `git diff --check`
  - Passed.
- No commit created.
- RUSTSEC ignore entries removed.
- rmcp server feature is not enabled.
- rmcp exact locked version is `2.2.0` because `2.2.2` is unavailable in the registry.
