# Receipt: Step 6.1 ToolExecutionService contract

Date: 2026-07-15
Branch: `feat/tool-execution-service`
Worktree: `/opt/worker/wt/prx-tool-execution-service`
Baseline: `49124c51787d939c05a377675b1929d21ec3a466`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Added the typed `ToolExecutionService` application boundary for native tools
  and MCP aliases. It owns the fixed descriptor/effect/policy/optional
  approval/sandbox/execute/audit/typed-outcome sequence.
- Added the small raw `ToolBackend` port and `LegacyToolAdapter`; existing
  `Tool` implementations remain unchanged behind the compatibility adapter.
  The service can also be assembled directly from future backend adapters.
- Added typed commands with UUIDv7 operation IDs and optional idempotency keys,
  authenticated runtime context, public descriptors, policy decisions,
  approval requests/decisions, sandbox permits, terminal statuses/outcomes,
  and mandatory audit records.
- Adapted the authoritative `SecurityPolicy` decision point without deleting or
  bypassing current ACL, autonomy, approval, or grant behavior. Read-only,
  supervised, and full modes retain their existing allow/ask/deny semantics.
- Runtime-only scope and approval fields are stripped from caller arguments and
  reconstructed from trusted context/approval adapters. Actual chat identity is
  preserved independently from the runtime session key.
- Existing native and MCP cancellation behavior is retained. Fixed the default
  named-execution cancellation adapter so an MCP alias invokes
  `execute_named(alias, ...)` instead of collapsing to the `mcp_call` root.
- Every terminal path, including unknown tool, policy denial, approval denial,
  sandbox denial, invalid arguments, failure, and cancellation, emits exactly
  one typed audit projection. The default tracing sink is observational and
  cannot rewrite a completed side effect.
- No Chat or Agent call site was migrated in this step; that is the explicit
  Step 6.2 boundary. Existing permission and approval paths remain intact.

## Red evidence and local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- The first MCP-alias regression run failed because the default cancellation
  path invoked the root `mcp_call` adapter name. The trait compatibility fix
  preserved named alias invocation; the exact test and complete focused module
  then passed.
- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed on the implementation tree
  with no reported warnings.
- `cargo test -p openprx --lib tools::execution::tests -- --nocapture` - 9
  passed, 0 failed, 5,645 filtered out. Coverage includes ordered native
  execution, trusted scope/grant normalization, MCP alias routing, all three
  security autonomy modes, policy/approval/sandbox denials, argument
  validation, unknown capability, cancellation, and exactly-once audit.
- `cargo test -p openprx --lib tools::traits::tests -- --nocapture` - 3 passed,
  0 failed, 5,651 filtered out.
- `git diff --cached --check` for the implementation - passed.

Per `verification-policy.md`, strict clippy, the full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live
acceptance, and a release build were not run. They remain GitHub delivery or
deployment gates, not local Step 6.1 gates.

## Scope and rollback

- Scope: one new execution-contract module, its public exports, the minimal
  named-alias cancellation compatibility correction, focused tests, and this
  receipt.
- Pre-receipt implementation diff: 1,374 insertions and 1 deletion across 3
  files.
- Pre-receipt implementation diff SHA-256:
  `b44e0cd8ce779c81bc7413dce98cffce09fadebbfaad11ccaa6ced65d9db5fb4`.
- Rollback: revert the local Step 6.1 commit before basing Step 6.2 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, or runtime activation was performed.
