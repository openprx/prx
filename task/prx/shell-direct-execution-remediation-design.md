# PRX Direct Shell Execution Remediation Design

Date: 2026-07-19

Status: **APPROVED FOR IMPLEMENTATION BY OPERATOR REQUEST**

Implementation branch: `fix/shell-direct-execution`

Target branch: `main`

## 1. Problem statement

PRX currently claims that `autonomy.level = "full"` permits unrestricted
command execution, but the deployed shell path still contains three independent
enforcement layers:

1. `ShellTool` receives `memory.acl_enabled` and rejects commands containing
   protected-memory path strings;
2. `SideEffectGate::authorize_command_execution` parses command text and rejects
   paths, shell variables, command substitutions, and wrappers before the shell
   sees them;
3. `ShellProcessAdapter` constructs and applies an OS sandbox and replaces the
   caller environment with a hardened PATH.

The always-on capability refactor removed `SandboxConfig.enabled` without
removing the sandbox. The active legacy `enabled = false` key is therefore
ignored and `SandboxConfig::default()` selects `backend = "auto"`. On Linux this
activates Landlock and causes deployed failures for `/dev/null`, the source
checkout, the active PRX configuration, Git, user toolchains, and normal shell
process-variable syntax.

This is a contract failure, not a missing allow-list entry. Adding more path or
syntax exceptions would retain the same broken architecture.

## 2. Required runtime contract

PRX host command execution is a trusted, direct execution facility. Once a
shell, Cron shell job, or Xin shell task reaches the command executor, PRX MUST:

- pass the complete command string unchanged to the configured runtime adapter;
- execute in the requested workspace directory;
- inherit the parent process environment and PATH;
- permit standard shell variables, substitutions, redirections, pipelines,
  compound commands, and background-process bookkeeping;
- permit normal host filesystem and device access, including `/dev/null`;
- preserve the real exit status and independently captured stdout/stderr;
- retain timeout, cancellation, process-group termination, bounded output, and
  audit/observability events.

The executor MUST NOT:

- inspect command text for ACL, path, variable, substitution, or wrapper rules;
- receive or consult `memory.acl_enabled`;
- construct, auto-detect, or apply an OS sandbox;
- clear or replace the process environment or PATH;
- silently reinterpret legacy sandbox configuration.

## 3. Scope

### 3.1 In scope

- `ShellTool` construction and execution.
- The shared `ShellProcessAdapter` used by interactive shell, Cron, and Xin.
- Cron/Xin command-execution admission checks that parse or reject command text.
- Removal of the host-shell sandbox configuration and factory wiring.
- Removal of host-shell-only sandbox backends and resolver code when no live
  caller remains.
- Configuration templates, schema exports, docs, tests, and migration behavior.
- Deployed Kimi K3 acceptance in tmux `demo`.

### 3.2 Explicit non-goals

- Memory record ownership/topic ACL semantics.
- `file_read` protection of memory database and snapshot files.
- Gateway pairing, API authorization, channel allowlists, or delegated-tool
  capability boundaries.
- WASM plugin permission declarations and plugin isolation.
- Resource lifecycle controls such as timeout, cancellation, output bounds, and
  process-tree cleanup.
- Causal Tree enablement semantics.

These mechanisms do not execute host shell commands and are not used to filter
the shell command stream.

## 4. Architecture change

### 4.1 Before

```text
tool factory
  -> ShellTool(SecurityPolicy, ACL flag, RuntimeAdapter, Sandbox, PATH grants)
  -> ACL string scan
  -> SideEffectGate command parser
  -> ShellProcessAdapter(RuntimeAdapter, Sandbox, PATH grants)
  -> env_clear + synthetic PATH
  -> Sandbox::wrap_command
  -> host shell
```

Cron and Xin independently repeat the `SideEffectGate` command authorization
before using the same sandbox-owning process adapter.

### 4.2 After

```text
tool factory
  -> ShellTool(RuntimeAdapter, workspace directory)
  -> ShellProcessAdapter(RuntimeAdapter)
  -> inherited environment and PATH
  -> host shell
```

Cron and Xin keep their scheduling, persisted state, lease, cancellation,
timeout, and result-commit contracts, but no longer parse or sandbox their shell
payload at execution time. Authorization to create or mutate a Cron/Xin task
remains at the tool/orchestration boundary.

## 5. Configuration and compatibility

`[autonomy.sandbox]` is removed from the generated configuration and typed
runtime schema because it no longer controls any host command execution path.
Legacy configuration containing this table remains parseable during the
transition, but it has no runtime effect. Documentation must state this
explicitly so an operator is not given a false isolation guarantee.

`memory.acl_enabled` remains a memory subsystem setting. It must not be passed
to `ShellTool`. This preserves memory ownership semantics without coupling them
to host command execution.

No replacement shell enable/disable switch is introduced.

## 6. Security and operational impact

This intentionally broadens host command authority after the orchestration
layer selects the shell tool or runs an accepted Cron/Xin shell payload. The
operator explicitly requested this trusted-host contract.

The following controls remain:

- tool availability and outer orchestration decisions;
- Gateway/channel authentication and pairing;
- command timeout and cancellation;
- child process-group termination and reaping;
- stdout/stderr capture limits;
- tool execution audit records and command outcome telemetry;
- Cron/Xin leases, idempotency, fencing, and durable terminal results.

The following controls are intentionally removed from the host command path:

- path and syntax deny logic;
- runtime approval grants evaluated inside command execution;
- memory-path ACL substring matching in shell commands;
- Landlock, Firejail, Bubblewrap, Docker command wrapping;
- synthetic PATH allow-lists.

## 7. Implementation plan

1. Refactor `ShellProcessAdapter` to own only `RuntimeAdapter` plus process
   lifecycle/output handling.
2. Stop clearing environment and stop constructing a synthetic PATH.
3. Refactor `ShellTool` to own only the process adapter and workspace path.
4. Remove internal ACL, rate/policy, approval-grant, and command-parser gates
   from `ShellTool`.
5. Remove command-payload policy gates from Cron/Xin execution while retaining
   task lifecycle controls.
6. Remove sandbox construction from all tool factories and background runners.
7. Remove obsolete sandbox schema/template/runtime modules and update exports.
8. Replace tests that assert command blocking with direct-execution contract
   tests.
9. Update configuration and troubleshooting references.
10. Run the complete validation and deployment gates below.

## 8. Regression matrix

### 8.1 Direct shell contract

| Case | Required result |
|---|---|
| `printf ok >/dev/null` | success |
| read `/opt/worker/code/prx/Cargo.toml` | success |
| read `~/.openprx/config.toml` | success |
| `git -C /opt/worker/code/prx status --short` | success |
| `cargo --version` and `rustc --version` | success |
| `sleep 0.01 & pid=$!; wait "$pid"` | success |
| `value=$(pwd); printf '%s' "$value"` | success |
| pipelines, `&&`, `||`, redirection | shell-native behavior |
| non-zero command | non-zero result with stderr retained |
| excessive output | bounded and marked truncated |
| timeout/cancellation | whole process group terminated and reaped |

### 8.2 Cross-feature regression

- Configuration load/merge/init and legacy config parsing.
- Chat/TUI session, streaming, tool calls, persistence, continue, and exit.
- Providers and routing, including Kimi K3 text/stream/tool calls.
- Memory store/recall/search/get and memory ACL-specific tests.
- MCP client/server tools and gateway endpoints.
- Hooks, WASM plugins, skills, browser/web tools, media, nodes, A2A.
- Cron create/list/run/history and real shell outcome.
- Xin system tasks, shell execution, leases, cancellation, and result commit.
- Gateway pairing, webhook, health/readiness, configuration API authorization.
- Causal Tree remains the sole intentionally optional capability.

### 8.3 Engineering gates

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo check --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Focused shell, runtime, Cron, Xin, configuration, tool-security, and Chat PTY
tests must run before the complete workspace suite. A test command that filters
out all intended tests is not accepted as evidence.

## 9. Merge and deployment gate

1. Commit the validated implementation branch.
2. Merge it into local `main` without touching unrelated untracked files.
3. Re-run formatting/check plus focused direct-shell tests on merged `main`.
4. Build `cargo build --release --locked` from the merged commit.
5. Back up `/home/ck/.cargo/bin/prx` with its hash.
6. Copy the new binary to a same-filesystem temporary path and atomically rename
   it over the deployed binary.
7. Verify version, source commit, binary hash, configuration parsing, migration,
   doctor, and service/process state.

## 10. Deployed K3 acceptance

Start the deployed `/home/ck/.cargo/bin/prx chat --plain -p kimi-code -m k3`
inside tmux session `demo`. K3 must run the regression matrix against the live
tool surface and report raw evidence, not infer success from source code.

Acceptance requires:

- all direct shell contract commands pass;
- source/config/Git/toolchain access is restored;
- Cron and Xin each provide a real run/result proof;
- memory, MCP, hooks, WASM/plugin, provider, routing, Gateway, and Causal Tree
  status are checked;
- no new panic, crash loop, permission denial, forbidden dynamic path, forbidden
  substitution, or sandbox error appears in runtime logs;
- every failure is either fixed and retested or recorded as an external
  dependency with reproducible evidence. Related implementation failures block
  completion.

## 11. Rollback

Rollback is an atomic binary restore plus source revert:

1. stop active PRX processes that hold the deployed executable;
2. atomically restore the recorded pre-deployment binary;
3. restart the prior runtime entrypoints;
4. revert the single remediation merge commit if source rollback is required;
5. verify binary hash/version and repeat the minimal tmux smoke test.

The deployment receipt must record the backup path, hashes, merge commit,
validation commands, test counts, and K3 transcript location.
