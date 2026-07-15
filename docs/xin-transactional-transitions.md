# Xin transactional transitions

Status: Step 8.2 implementation baseline

Xin now has one local commit boundary for execution results and a durable,
idempotent delivery boundary for the shared memory event spine. The design does
not claim atomicity across two SQLite databases: local truth commits first, and
the shared mirror is recoverable from an outbox row committed with that truth.

## Legacy task execution commit

`commit_task_execution` replaces the runner's former sequence of independent
repository calls. One `BEGIN IMMEDIATE` transaction now owns:

- the completed or failed task result and bounded output;
- run and failure counters, including failure-based disabling;
- the `xin_runs` history row;
- recurring-task reschedule state and `next_run_at`;
- result, run-recorded, and rescheduled lifecycle events; and
- one durable outbox row for every lifecycle event.

The transition requires the task to still be `running`. A missing or no-longer
running task returns `false`, and any statement failure rolls back the entire
unit. The runner reports an execution as successful only when both the work and
this local commit succeed. The former separate result, run-record, and
reschedule helpers are test-only fixtures and are absent from production builds.

## Step result and goal progress

Lease-fenced step completion and failure already use an immediate transaction.
Their step result, terminal/retry event, goal progress recomputation, goal
terminal event, and corresponding outbox rows therefore commit or roll back
together. Fault-injection coverage verifies that an outbox insert failure leaves
the running step, its lease, and the pre-completion goal progress unchanged.

## Event outbox contract

Every local `xin_task_events` insert and its `xin_event_outbox` enqueue are an
atomic savepoint pair, whether the caller already owns a larger transaction or
uses an autocommit connection. An outbox row contains the stable event ID,
workspace, subject task/goal, event type, enriched lineage payload, creation
time, delivery state, attempt count, and last bounded error.

After each Xin repository operation, pending rows are offered to
`memory/brain.db`. Delivery uses the outbox event ID with `INSERT OR IGNORE`, so
a crash after the external insert but before the local `delivered_at` update is
safe to replay. A delivery failure does not undo committed local truth; it is
recorded on the outbox row and retried by the next repository access.

## Boundaries

- SQLite cannot provide one transaction across `xin/tasks.db` and
  `memory/brain.db`; recovery and stable-ID idempotency are the explicit
  guarantee.
- The drain is opportunistic and bounded to 100 rows per repository access. A
  dedicated background dispatcher is not introduced in this step.
- Ordered-step prerequisites and legacy-task adoption semantics belong to Step
  8.3. Heartbeat retirement belongs to Step 8.4.
- GitHub delivery and release gates remain governed by
  `verification-policy.md`.
