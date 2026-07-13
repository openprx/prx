# Receipt: Step 2.3 shared shell-process parity and Xin lease safety

Date: 2026-07-13  
Branch: `fix/cron-process-parity`  
Worktree: `/opt/worker/wt/prx-cron-process-parity`  
Baseline: `4842f9323a4ca9302c712c0b4e4779a12d155b80`  
Status: implementation and local verification complete; prepared for a local-only commit and not pushed, deployed, or activated

## Delivered contract

- Added the policy-free `runtime::shell_process::ShellProcessAdapter` and routed
  interactive shell, Cron shell jobs, and Xin shell steps through it. Those
  production entrypoints no longer build or spawn raw child processes.
- The adapter builds the configured runtime, resolves `extra_path_dirs` once for
  both sandbox grants and `PATH`, clears the ambient environment, restores only
  the explicit safe-variable allowlist, and installs a hardened non-empty base
  `PATH`. Arbitrary parent secrets are not inherited.
- Sandbox wrapping is applied after runtime construction and fails closed. The
  configured `runtime.kind` is therefore honored consistently by Shell, Cron,
  and Xin instead of Cron/Xin silently taking a native-only path.
- Unix children run in a dedicated process group. Timeout, cancellation, and
  execution-future drop kill the group and retain ownership long enough to reap
  the direct child. A pre-cancelled request is rejected before runtime build or
  spawn.
- Stdout and stderr are drained concurrently, retained independently at 1 MiB,
  and receive a stable UTF-8-safe truncation marker. After forced termination,
  output drain has a one-second bound so a re-parented `setsid` descendant that
  keeps a pipe open cannot strand the caller or reader tasks. The coordinator
  owns abort-on-drop handles for both readers; aborting and awaiting the outer
  coordinator therefore cannot detach either reader.
- Shell keeps its 60-second timeout and legacy user-visible error/truncation
  strings. Cron and Xin use 120 seconds.
- Forbidden-path validation moved into `SecurityPolicy` and now applies to all
  three entrypoints in both Full and Supervised autonomy. Its quote-aware word
  and redirection tokenizer recognizes both `cat </etc/passwd` and
  `cat</etc/passwd` without treating `<`/`>` inside quoted prose as operators.
  Typed tokens also retain shell-expansion context: dynamic executables,
  dynamic redirection operands, and dynamic arguments fail closed by default.
  Unquoted or double-quoted backticks and `$(` command substitution, plus
  unquoted `<(` and `>(` process substitution, are separately typed as active
  substitution and fail closed before legacy structural command policy runs.
  Before structural, path, wrapper-recursive, or risk parsing, POSIX
  backslash-newline continuations are folded quote-aware: LF and CRLF fold in
  unquoted/double-quoted contexts, single-quoted content stays literal, and an
  even backslash run does not consume the physical line ending.
  Only no-slash, non-redirection dynamic arguments to the explicit benign
  `echo`, `printf`, and `sleep` bases remain compatible. Dollar signs inside
  single quotes remain literal unless the containing value is recursively
  interpreted as a wrapper/interpreter command payload.
  Each rejection emits exactly one gate audit with the correct `shell`,
  `cron_scheduler`, or `xin_runner` tool name.
- Cron manual runs now use a private typed authorization state. The tool path
  performs the single side-effect gate and single action-budget charge, then
  enters the scheduler through an explicitly preauthorized path. Background
  scheduler paths still perform their own authorization.
- Xin claim, mark-running, heartbeat renewal, checkpoint, and terminal writes
  carry one typed `XinStepLease` generation consisting of worker id plus exact
  expiry. Claim and mark-running acquire `BEGIN IMMEDIATE` before sampling the
  authoritative SQLite time; mark and renew require the exact generation and a
  non-expired lease. A successful renewal replaces the token; a rejected
  renewal or confirmed deadline cancels the complete in-flight step future.
  Transient renewal errors do not extend local authority.
- Xin checkpoint, completion, and failure writes are fenced by exact owner and
  exact expiry plus an authoritative SQLite non-expiry check inside `BEGIN
  IMMEDIATE`. Authority loss writes no checkpoint, success, failure, retry, or
  stale status update. A same-worker reclaim with a new expiry is a distinct
  generation and rejects writes carrying the old expiry.

## Red-first evidence

The architecture allowlist was first changed to require the sole raw shell
spawn to live in `src/runtime/shell_process.rs`, before the three existing
entrypoints were migrated. The focused guard failed with exit 101 after finding
the old raw sites in:

- `src/tools/shell.rs`
- `src/cron/scheduler.rs`
- `src/xin/runner.rs`

That run took 5m26s and established the process-parity boundary before the
implementation. The final guard accepts only the shared adapter site and adds a
second source guard that requires all three entrypoints to delegate to the
adapter without raw `Command`, `spawn`, or `output` calls.

## Focused green evidence

- Shared process adapter: 9 passed, including hardened PATH/environment,
  sandbox application, pre-cancellation, concurrent bounded stdout/stderr,
  timeout/cancellation group kill, future-drop group kill, leader-exit pipe
  handling, and bounded drain with a `setsid` descendant.
- `tools::shell::tests`: 27 passed, 0 failed, 1 pre-existing ignored. Legacy
  error and truncation-marker snapshots passed.
- `cron::scheduler::tests`: 29 passed.
- `tools::cron::tests`: 32 passed. The real risky V2 one-shot manual run
  succeeded, exhausted its one-action budget exactly once, and produced exactly
  one gate audit.
- `xin::runner::tests`: 15 passed. This includes configured runtime selection,
  sandbox fail-closed behavior, tool-identity audit, and real lease loss where
  a new owner reclaimed the step while the old process was cancelled without a
  stale marker or state mutation.
- `xin::store::tests`: 37 passed. A real `BEGIN IMMEDIATE` lock was held across
  expiry and the delayed renewal was rejected; a second real lock-wait test
  proved claim TTL starts after lock acquisition. An expired claim cannot enter
  running, and same-process/same-worker reclaim rejects the retained old
  generation's mark, renewal, and marker side effect.
- `security::policy::tests`: 132 passed. Both attached and separated forbidden
  input-redirection forms are denied in Full and Supervised modes, quoted prose
  remains literal, and six gate calls produce exactly six deny audit events
  across the three shell entry identities.
- Cron and Xin runtime-spy tests passed, proving both use the configured runtime
  builder. Unavailable sandbox tests passed fail closed.
- `raw_child_process_spawns_are_explicitly_allowlisted` — PASS on the final
  source tree.
- `shell_entrypoints_delegate_process_execution_to_shared_adapter` — PASS on
  the final source tree. The guard extracts the three production function
  bodies by brace depth instead of truncating at the first `cfg(test)`, and
  rejects raw `Command`, `spawn`, `output`, and `status` execution paths.

## Third-review closure

- Reader lifecycle: `OutputDrainTasks` retains both reader `JoinHandle`s while
  joining and aborts every remaining handle on drop. Grace expiry aborts and
  awaits the coordinator before returning. The `setsid` regression has a
  per-adapter active-reader Drop canary and observes zero active readers at the
  bounded return point.
- Reap authority: `terminate_and_reap` returns whether the direct leader was
  actually reaped. Cancel/timeout marks the process owner complete only on
  confirmed reap; a `wait` error leaves Drop/background cleanup armed for
  another ownership-preserving reap attempt.
- Claim clock: both claim and mark-running acquire the SQLite write reservation
  before sampling authoritative time. A claim blocked for 2.2 seconds receives
  a fresh post-lock two-second TTL rather than persisting an already-expired
  pre-lock deadline. Mark-running rejects an expired token.
- Typed generation: production no longer passes a bare worker id through the
  Xin execution lifecycle. Exact owner+expiry tokens fence mark-running and
  renewal as well as the existing checkpoint/finish writes; legacy bool/expiry
  wrappers are compiled only for tests.
- Forbidden redirection: quote-aware tokenization splits unquoted attached or
  separated redirection operands, closing both reviewed `/etc/passwd` bypasses
  while preserving quoted redirection-like prose.
- Architecture guard: source validation now targets the actual production
  function bodies and includes `.status(` among forbidden raw execution calls.

## Fourth-review closure

- Dynamic expansion: the tokenizer now emits typed word, redirection, and
  command-separator tokens with a per-word dynamic-expansion bit. `$` inside
  single quotes stays literal; `$` in double quotes or unquoted text is dynamic.
- The fourth-round dynamic policy rejected a dynamic executable, every dynamic
  redirection operand, every dynamic token with a literal `/`, and no-slash
  dynamic arguments to a finite path-consuming command set. This closed
  `cat "$HOME/.ssh/id_rsa"`, `X=/etc/passwd; cat <$X`, and `cat $FILE` in both
  Full and Supervised autonomy while preserving `echo '$HOME/.ssh'` as a
  literal.
- Audit cardinality: `cat $FILE` through `shell`, `cron_scheduler`, and
  `xin_runner` produces exactly three deny events for three calls. The complete
  SecurityPolicy module passes 126 tests.
- Compatibility characterization: the first blanket dynamic-argument rule made
  four existing ShellTool tests red because normal `echo $HOME`, `echo $PATH`,
  `${OPENPRX_API_KEY:-unset}`, and `$!` usage was denied. No fixture weakening
  was retained. The policy was narrowed by command/operand semantics, the
  original commands and assertions were restored, and the final ShellTool
  module passed 27 tests with its one pre-existing ignored test.
- Architecture chain: the guard now extracts both the public wrapper and the
  actual execution helper for Cron and Xin, asserts the wrapper calls that exact
  inspected helper, and requires `.execute(` plus `ShellProcessRequest` inside
  each real helper. Raw `Command`, `spawn`, `output`, and `status` patterns are
  rejected over the complete inspected chain. The final focused guard passed.

## Fifth-review closure

- Closed-world default: the finite path-consuming allowlist was removed.
  Dynamic executables and arguments now deny by default. The only dynamic
  argument compatibility exception is an ordinary, no-slash, non-redirection
  argument to `echo`, `printf`, or `sleep`; the original `$HOME`, `$PATH`, API
  key fallback, and `$!` ShellTool fixtures remain unchanged and green.
- Wrapper recursion: `command`, `eval`, and `env` command portions plus
  `sh`/`bash`/`dash`/`zsh`/`ksh`/`fish -c` payloads are recursively validated by
  the same typed tokenizer. Recursion is capped at four levels and fails closed
  at the bound. Single-quoted wrapper payloads therefore cannot hide `$FILE`.
- Code interpreters: `python`, `python3`, `perl`, `ruby`, `node`, and `php`
  `-c`/`-e` payloads reject dynamic `$` content and extracted absolute or
  forbidden literal paths. Ordinary allowed relative script arguments remain
  compatible.
- Focused bypasses: `FILE=/etc/passwd; command cat $FILE`,
  `FILE=/etc/passwd; eval 'cat $FILE'`,
  `FILE=/etc/passwd sh -c 'cat "$FILE"'`, and a Python `open("/etc/passwd")`
  payload all deny in Full and Supervised autonomy. The `eval` form produces
  exactly one deny event for each of `shell`, `cron_scheduler`, and
  `xin_runner`.
- Compatibility correction: an intermediate blanket wrapper-slash rule made
  the existing `sh ./retry-once.sh` Cron retry fixture red. The fixture was
  restored unchanged; literal relative/allowed script paths now use the normal
  `is_path_allowed` path, while only interpreter command/code payloads recurse.
  The original fixture and full Cron scheduler module are green.
- Final module suites: SecurityPolicy 126, ShellTool 27 passed with one
  pre-existing ignored test, shared adapter 9, Cron scheduler 29, Cron tool 32,
  Xin runner 15, and Xin store 37; no failures.

## Sixth-review closure

- Active substitution is no longer inferred from raw substring searches. The
  typed tokenizer marks unquoted/double-quoted backticks and `$(` as command
  substitution, and unquoted `<(`/`>(` as process substitution. Escaped forms
  and identical prose inside single quotes remain literal.
- The forbidden-path/substitution gate now runs before the legacy structural
  policy, so every entrypoint gets the same fail-closed result in both Full and
  Supervised autonomy without executing a child command. The user-facing deny
  reason is generic and does not echo the substitution payload.
- Actual backtick substitution, `echo "$(cat /etc/passwd)"`, and process
  substitution were denied through `shell`, `cron_scheduler`, and `xin_runner`.
  Three forms across three tools produced exactly nine deny audit events.
- `${VAR}` retains the existing dynamic-variable semantics rather than being
  classified as command substitution: a malicious dynamic path remains denied,
  while the established benign `echo ${SAFE:-unset}` shape remains allowed.
- Final module suites after this closure: SecurityPolicy 128, ShellTool 27
  passed with one pre-existing ignored test, shared adapter 9, Cron scheduler
  29, Cron tool 32, Xin runner 15, and Xin store 37; no failures.

## Seventh-review closure

- Added `fold_shell_line_continuations` as the common pre-parse normalization
  for structural command checks, forbidden-path/substitution checks (including
  recursively interpreted shell-wrapper payloads), and risk classification.
  It removes a final unescaped backslash plus LF or CRLF in unquoted and
  double-quoted contexts, preserves single-quoted text, and counts backslash
  runs so only an odd run consumes the physical line ending.
- The two reviewed physical-line forms use the `echo $` and `cat <` prefixes
  with one trailing backslash before a next-line `(printf secret)`. They now
  fold to active command/process-substitution operators and deny in Full and
  Supervised autonomy. Both LF and CRLF variants passed.
- Direct `$(` and `<(` forms remain denied. Single-quoted backslash-newline
  substitution-like prose remains allowed. Additional tests prove structural
  `find -exec`, forbidden `/etc/passwd`, and high-risk `rm` tokens cannot be
  split across a continuation to evade their respective parsers.
- The gate still receives and audits the original command, not the normalized
  copy. Four continued forms across `shell`, `cron_scheduler`, and `xin_runner`
  produced exactly 12 deny events, each containing the exact original physical
  command including its LF or CRLF.
- Final module suites after this closure: SecurityPolicy 132, ShellTool 27
  passed with one pre-existing ignored test, shared adapter 9, Cron scheduler
  29, Cron tool 32, Xin runner 15, and Xin store 37; no failures. Both process
  architecture guards also passed.

## Strict final gates

All Cargo commands used
`CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp`.

- `cargo clippy --all-targets --all-features -- -D warnings` — PASS on the
  seventh-review source tree, 1m11s. Its first run caught three indexing lints
  in the new folding helper; those accesses were converted to safe `.get()`
  handling, the exact quote/CRLF/parity regression passed, and the clean rerun
  passed.
- `cargo check --all-targets` — PASS, 2m59s.
- `cargo check --no-default-features` — PASS on the seventh-review source tree,
  17.46s.
- `cargo check -p openprx --lib` — PASS on the seventh-review production source,
  9.17s.
- `cargo fmt --all -- --check` — PASS on the final source tree.
- `git diff --check` — PASS on the final source tree.

## Root formal-gate closure

- `cargo clippy --workspace --all-targets --all-features -- -D warnings` first
  passed on the seventh-review tree in 4m57s.
- The first formal `cargo test --bin prx --all-features` ran 5,611 tests and
  exposed three stale fixtures: two PTY commands used syntax now correctly
  rejected by the shared substitution/path policy, and one Cron Full-mode test
  still expected `/tmp` output to bypass the shared forbidden-path boundary.
  The fixtures were changed without weakening coverage: the PTY output loop now
  uses `awk`, the orphan-PTY case keeps its `setsid` child without `/dev/null`,
  and the Cron test writes a workspace-relative path. All three focused tests
  passed, followed by the full binary suite at 5,604 passed, 0 failed, and 7
  ignored.
- The first formal architecture run passed four guards and correctly rejected
  four new direct SQLite opens inside Xin lease tests. Those fixtures were
  consolidated behind the reviewed `open_xin_test_connection` repository-test
  helper, with one explicit architecture allowlist entry. The three affected
  lease tests passed and the architecture suite then passed 5/5.
- After the fixture/helper adjustments, the final
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  rerun passed in 1m11s; `cargo fmt --all -- --check` and `git diff --check`
  also passed.
- Two independent final reviewers reported no High or Medium findings: one
  covered process ownership/Xin fencing and one covered PGID cleanup plus the
  final line-continuation policy closure.

## Explicit runtime boundary

The safe child environment intentionally excludes every `DOCKER_*` variable,
matching the ambient-secret non-inheritance contract. Consequently the Docker
runtime path in this step supports the default Docker CLI/socket environment;
a custom `DOCKER_HOST`, TLS certificate directory, or Docker config inherited
from the parent is not claimed as supported. Supporting that case requires a
future RuntimeAdapter-scoped outer-environment contract rather than weakening
the environment allowlist for executed workloads.

On Unix the adapter owns and kills the process group it created. A descendant
that deliberately creates a new session/process group with `setsid` is outside
that PGID (and, without an external cgroup/subreaper boundary, outside the
adapter's kill authority). This step claims bounded return and zero leaked
reader tasks for that case, not termination of the escaped descendant. The
regression fixture explicitly kills its escaped `setsid` process during test
cleanup.

No live runtime, scheduler service, deployed binary, Docker daemon, or active
configuration was mutated during this handoff.

## Files changed

- `src/runtime/shell_process.rs`
- `src/runtime/mod.rs`
- `src/security/mod.rs`
- `src/security/policy.rs`
- `src/tools/shell.rs`
- `src/chat/sessions/pty.rs`
- `src/cron/mod.rs`
- `src/cron/scheduler.rs`
- `src/tools/cron.rs`
- `src/xin/runner.rs`
- `src/xin/store.rs`
- `tests/architecture_boundaries.rs`
- `collab-outbox/receipt-2026-07-13-cron-process-parity.md`

## Handoff boundary

This receipt is included in the local Step 2.3 changeset. No push, deployment,
service restart, `prx init`, active-workspace mutation, or live tmux acceptance
was performed. Per the user pause boundary, execution stops after this local
commit and resumes at Step 2.4 only when explicitly requested.
