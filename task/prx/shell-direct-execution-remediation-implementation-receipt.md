# PRX Shell Direct Execution Remediation — Implementation Receipt

Date: 2026-07-19

Design: `task/prx/shell-direct-execution-remediation-design.md`

## Delivered code boundary

- Removed all host-shell command authorization, ACL checks, command-text parsing,
  action-budget rechecks, environment clearing, synthetic PATH construction, and
  OS sandbox wrapping from `ShellTool`, background `/shell`, PTY, Cron shell, and
  Xin shell execution.
- Kept process lifecycle controls: working directory, bounded output, exit status,
  timeout, cancellation, process-group termination, and caller-level tool audit.
- Removed `SandboxConfig`, `SandboxBackend`, sandbox factories/backends, Landlock
  dependency/features, and generated `[autonomy.sandbox]` configuration.
- Kept non-shell controls outside the requested boundary: file-tool policy,
  memory record ACL, gateway/channel authentication, remote-node boundaries, and
  WASM plugin capability controls.
- Bumped the patch release from 0.8.14 to 0.8.15 and added release notes.

## Code-level regression evidence

Focused suites:

- ShellTool: 6 passed.
- shared shell process adapter: 8 passed.
- background shell sessions: 9 passed.
- `int_tool_security`: 29 passed.
- `int_p1_cross_module`: 21 passed.
- Cron scheduler: 29 passed.
- Xin runner: 20 passed.
- `chat_pty_e2e`: 31 passed, 1 pre-existing environment-dependent test ignored.

Full engineering gate:

- `cargo fmt --all -- --check`: passed.
- `cargo check --workspace --all-features`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo test --workspace --all-features`: passed. The main library ran 5,722
  tests with 5,716 passed and 6 ignored; all integration and doctest binaries
  subsequently completed with zero failures.
- `git diff --check`: passed.

Behavioral assertions include `/dev/null`, variables, command substitution,
pipelines, background PID/wait, inherited PATH and environment, reads outside the
workspace, non-zero status/stderr, timeout, cancellation, and process-group kill.

## Delivery and deployed acceptance

Initial delivery reached local `main` at merge commit `1b4155af`; the release
binary was installed as `/home/ck/.cargo/bin/prx` and identified itself as
`prx 0.8.15`.

The first deployed K3 run in `tmux demo:2.0` completed its full matrix with the
marker `K3_PRX_0815_FULL_SELF_CHECK_DONE`. It verified direct host Shell behavior,
memory store/recall/get/forget, Cron create/run/history/remove, Xin, Gateway,
daemon, doctor, nodes, managed sessions, web search, provider streaming, and
tool-call closure. MCP, hooks, WASM plugins, media, and external nodes were
reported as optional and unconfigured rather than runtime failures.

That run exposed two post-deploy defects which were then fixed:

- the generic tool readiness stage still emitted the misleading audit field
  `sandbox=adapter_owned_chat_dispatch`; types, states, fields, tests, and trace
  output now consistently call this an execution `preparation` stage;
- the memory phone detector treated a bare technical PID such as `3913571` as
  PII and rejected chat-session persistence. Bare technical counters are now
  accepted, while E.164, conventionally separated, and explicitly phone-labeled
  numbers remain rejected.

After these fixes the full gate was repeated. The main library now contains
5,724 tests: 5,718 passed and 6 pre-existing tests were ignored; every integration
test and doctest completed with zero failures. A final rebuild, main merge,
deployment, and tmux K3 persistence/audit replay follow this receipt update.
