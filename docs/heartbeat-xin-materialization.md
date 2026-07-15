# HEARTBEAT.md to Xin materialization

Status: Step 8.4 implementation baseline

`HEARTBEAT.md` remains the human-editable source. It no longer owns an
execution loop: parsed bullets are reconciled into recurring Xin tasks, and the
Xin runner is the sole claim/execution/result/reschedule owner.

## Stable materialization

Each parsed dash bullet becomes a `system`/`agent_session` Xin task whose name
is `heartbeat:` plus the SHA-256 digest of the normalized bullet text. The task
payload is the existing configured heartbeat prompt followed by the existing
`[Heartbeat Task]` text. The recurring interval keeps the historical five-minute
minimum.

This identity has the following behavior:

- restart and file reordering retain the same Xin task ID;
- prompt, interval, description, and enablement changes update the existing
  task rather than creating a replacement;
- a new task is due immediately, matching the former Tokio interval's immediate
  first tick;
- removed bullets are disabled, not deleted, preserving run/event history;
- re-adding an identical bullet re-enables the same task; and
- a no-change reconciliation writes no lifecycle-event churn.

Reconciliation is idempotent and recoverable. Individual ensure/update writes
use the existing Xin repository boundaries, and every Xin poll retries the full
desired-state comparison. Namespace dematerialization is one immediate
transaction with lifecycle/outbox events.

## One scheduler

The daemon no longer spawns `run_heartbeat_worker`, and it no longer loops over
bullets calling `agent::run` directly. When either Xin or Heartbeat is enabled,
one supervised Xin runner starts. Its poll interval is the faster required
interval of the enabled domains.

If Heartbeat is enabled while Xin is disabled, the runner operates in
heartbeat-only mode: it reconciles and executes only `heartbeat:` tasks. It
does not drive Xin goals, adoption, built-in Xin tasks, or ordinary legacy Xin
tasks. When Xin is enabled too, both domains share the same runner.

The Heartbeat health component remains visible but is owned by Xin and marked
healthy after successful reconciliation. The existing active-hour window is
checked before heartbeat tasks are admitted; due tasks wait until the window
opens.

## Inherited guarantees

Materialized tasks use the normal Xin claim and execution path, including
security policy, shared Agent runtime envelope, run history, failure counts,
output bounds, transactional result/event/outbox commit, recurring reschedule,
and stable memory/audit/terminal scope. There is no second Heartbeat result or
scheduling ledger.

## Compatibility and boundary

- HEARTBEAT.md parsing, default file creation, configured prompt, interval, and
  active-hours semantics remain supported.
- Existing materialized tasks are disabled on the next Xin reconciliation when
  Heartbeat is disabled or a bullet disappears.
- No runtime was installed or activated by this implementation step.
