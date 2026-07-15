# Receipt: Step 6.4 permission simplification

Date: 2026-07-15
Branch: `feat/permission-simplification`
Worktree: `/opt/worker/wt/prx-permission-simplification`
Baseline: `35579755f3359cd52718c120555b7fc459a4a3e5`
Status: implementation and local verification complete; local commit pending;
not pushed, merged, deployed, or activated

## Delivered contract

- Changed the product default from `Supervised` to `Full`: an installed and
  advertised executable capability now runs without a confirmation prompt by
  default. The default remains workspace-scoped, keeps forbidden-path rules,
  sandbox configuration, rate limits, scope ACLs, trusted runtime fields,
  signed runtime grants, and side-effect audit.
- Updated generated server configuration to select `full` while retaining
  `workspace_only = true`. Minimal and missing autonomy configuration now
  deserialize to the same autonomous default.
- Kept `Supervised` as the explicit opt-in confirmation mode and `ReadOnly` as
  the deny-effects mode. Tests that exercise confirmation now select
  `Supervised` explicitly rather than inheriting a product default.
- Added `decide_tool_execution`, the canonical projection from
  `SecurityPolicy::decide` to the typed execution decision. Both the Agent
  serial path and Chat's `ToolExecutionService` policy adapter call it, so
  identical tool, principal, channel, chat type, policy, and scope inputs
  cannot be reinterpreted by an entry point.
- Retained `ToolExecutionService`'s mandatory typed audit. The default Full
  conformance test proves the decision is `Allow`, the installed Act tool is
  invoked without an approval strategy, and the emitted audit is
  `Allow/Succeeded/Act`.
- Reduced `ApprovalManager` to one responsibility: session-local interaction,
  the `Always` allowlist, and confirmation-decision audit. It no longer owns an
  autonomy copy or a second `needs_approval` decision function.
- Deleted the unused generated-grant cache and best-effort channel-derived
  `ApprovalGrantV2` bridge. Nothing consumed that cache. Runtime grants remain
  minted from the complete typed command and trusted execution context after
  approval, and tool-level gates still validate binding, expiry, scope, and
  single-use semantics.
- Removed all production callers of `ApprovalManager::from_config` and
  `from_autonomy_level`. Agent, Chat, and spawned-session flows construct the UI
  owner without giving it permission-policy ownership.
- Preserved explicit Supervised denial/approval behavior for Agent, MCP,
  gateway mutation, sessions spawn, subagent kill, memory deletion, and
  evolution commits. Preserved ReadOnly and scope denial behavior.

## Exit-gate evidence

- The execution-policy adapter test compares the Agent-facing canonical helper
  and Chat service adapter for Full, Supervised, and ReadOnly, including both
  Read and Act tools; every paired decision is identical.
- Agent tests prove a default Full installed Act tool executes without an
  approval manager, explicit Supervised Act is denied without one, explicit
  Supervised Read executes, and an `Always` allowlisted Act executes without a
  second prompt.
- Chat's Full-mode decision test returns `Allow` for the same side-effecting
  capability class.
- The ToolExecutionService test proves the shared default decision is preserved
  in the typed terminal audit record. Once allowed, both entry points invoke the
  same registered tool implementation, whose retained SideEffectGate produces
  the same security audit decision for identical operation inputs.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed on the final tree.
- `cargo check -p openprx --all-features` - passed on the final tree with no
  reported warnings.
- `cargo test -p openprx --lib tools::execution::tests` - 11 passed, 0 failed;
  includes canonical policy parity and typed default-Full audit.
- `cargo test -p openprx --lib approval::tests` - 20 passed, 0 failed; includes
  ApprovalManager UI/audit/allowlist plus Chat approval resolver tests.
- MCP signed-grant call paths - 4 passed, 0 failed.
- Memory-forget scope, grant, ReadOnly, and rate-limit paths - 8 passed,
  0 failed.
- Explicit Supervised evolution pipeline and manual-trigger denial - 2 focused
  tests passed, 0 failed.
- Explicit Supervised sessions-spawn denial, subagent-kill denial, gateway
  mutation denial, and gateway matching-grant success - 4 focused tests passed,
  0 failed.
- Agent default Full / explicit Supervised decision and execution paths -
  4 focused tests passed, 0 failed.
- Chat Full-mode policy path - 1 passed, 0 failed.
- Server template and minimal-config default tests - 2 passed, 0 failed.
- Shared-config hot authorization flip integration - 1 passed, 0 failed.
- Two initial focused runs exposed test fixtures that still assumed the old
  implicit Supervised default (`subagents` and `gateway`). Both fixtures were
  changed to explicit Supervised policies and their exact tests passed on the
  final tree; the failed runs are not acceptance evidence.
- `git diff --check` - passed.
- Source search found no remaining generated-grant cache, grant bridge,
  `ApprovalManager::needs_approval`, `ApprovalManager::from_config`, or
  `ApprovalManager::from_autonomy_level` implementation/call site.

Per `verification-policy.md`, strict clippy, the full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live TUI
acceptance, and a release build were not run. They remain GitHub delivery or
deployment gates, not local Step 6.4 gates.

## Scope and rollback

- Scope: product autonomy default, generated server template, canonical
  permission decision projection, dead approval ownership removal, explicit
  confirmation-mode fixtures, focused conformance tests, and this receipt.
- Pre-receipt implementation diff: 310 insertions and 383 deletions across 24
  files.
- Pre-receipt diff SHA-256:
  `60316f97ae53636b8f61b72a513e59c2ab460db8fd6ad50d00083d5d8c9f3fcb`.
- Rollback: revert the local Step 6.4 commit before basing Step 7.1 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, or runtime activation was performed.
