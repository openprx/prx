# Receipt: Step 7.2 Chat joins the shared Agent turn owner

Date: 2026-07-15
Branch: `feat/chat-agent-turn-owner`
Worktree: `/opt/worker/wt/prx-chat-agent-turn-owner`
Base: `e0740a468961536185c8c1ffc2a2a6411ad35152`
Status: implementation and local verification complete; recorded by the local
Step 7.2 commit; not pushed, merged, deployed, routed into an installed binary,
or activated

## Delivered

- Routed the production Chat provider/tool path through the existing
  `agent::loop_::run_tool_call_loop_outcome` owner. The production
  `drive_start_turn_stream` is now a Redux adapter; the historical duplicate
  loop is test-only.
- Added neutral live events for text, reasoning, retries, exact compaction
  patches, tool start/finish, and progress. Chat maps them to its existing
  actions without adding another provider or tool loop.
- Added true streaming support to the shared owner while preserving buffered
  `chat_traced` behavior for other entries. Streaming owns tool-call chunk
  aggregation, usage, network backoff, cancellation, and overflow recovery.
- Routed all production tool execution through `ToolExecutionService`.
  Non-TUI entries assemble the shared service from their registry/policy;
  Chat injects its existing TUI approval, sandbox, and audit strategies.
- Preserved priority scheduling, bounded parallel read-only batches, timeouts,
  serial stateful barriers, rollback-to-serial thresholds, observer events,
  repeated-failure termination, and native call IDs.
- Standardized provider-bound tool-result history on one canonical payload.
  UI-only success state remains in Redux tool events.
- Kept Redux preflight compaction/injection diagnostics, reducer state,
  ordered persistence, task-scoped finalization, and one terminal action.
  Cross-entry terminal commit remains explicitly deferred to Step 7.3.
- Updated the Step 7.1 characterization document into the Step 7.2 routed
  architecture baseline and recorded the remaining ownership boundary.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib step_7_1_same_` - 3 passed, 0 failed.
- `cargo test -p openprx --lib run_tool_call_loop_` - 9 passed, 0 failed.
- `cargo test -p openprx --lib supervised_` - 29 passed, 0 failed.
- `cargo test -p openprx --lib driver_` - 27 passed, 0 failed.
- `git diff --check` - passed.

The first broad `driver_` sweep found two real adapter gaps: streaming tool
specifications were capability-gated away, and structured tool calls without
a registry did not fail immediately. Both were fixed at the shared-owner
boundary; their two exact tests and the final 27-test sweep passed. Earlier
characterization/preflight/max-iteration/overflow failures were intermediate
diagnostics while routing and are not acceptance results.

Per `verification-policy.md`, strict clippy, full workspace/binary suites,
architecture guards, dependency/security audits, independent review, live TUI
acceptance, and release builds were not run. They remain GitHub delivery or
release/deployment gates.

## Scope and rollback

- Scope: shared turn-owner streaming/event/service extension, Chat adapter,
  production call-site registry sharing, characterization updates, and this
  receipt.
- No Stage 7.3 shared terminal commit or Stage 8 work was implemented.
- Rollback: revert the local Step 7.2 commit before basing Step 7.3 on it.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.
