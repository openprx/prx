# Scheduled work

The daemon runs two schedulers. Cron owns calendar, timezone, and delayed jobs.
Xin owns interval tasks and ordered goals. `HEARTBEAT.md` is a human-editable
checklist; the Xin runner materializes its bullets as recurring Xin tasks and
is the only claim, execution, and reschedule owner for that work.

## What runs

Cron jobs are `shell` or `agent`. A job schedule is a cron expression (optional
IANA timezone), a one-shot RFC3339 instant, or a fixed millisecond interval.
The scheduler polls at `reliability.scheduler_poll_secs`, with a five-second
floor. Due jobs are spawned and the poll loop continues; a long job does not
block the next due scan. `[cron] max_run_history` (default `50`) bounds retained
run rows.

Xin starts with the daemon. One runner ticks at the shorter of the Xin
interval (minimum one minute) and the Heartbeat interval (minimum five
minutes); the default tick is five minutes. Each tick reconciles
`HEARTBEAT.md`, marks stale work, drives runnable goal steps, and executes due
tasks. Task execution modes are `internal` (built-in Rust handlers),
`agent_session`, and `shell`.

Startup registers three built-in system tasks: `xin:health_check` (five
minutes), `xin:stale_cleanup` (thirty minutes), and `xin:memory_hygiene` (twelve
hours). Persisted `xin:memory_evolution` and `xin:fitness_report` names are
disabled; those jobs now belong to the self-system workers. Recurring task
failures increment a failure counter and reschedule; they do not disable the
task.

Heartbeat has no separate enable flag and no separate execution loop. Defaults
are `interval_minutes = 30`, `active_hours = [8, 23]`, and
`prompt = "Check HEARTBEAT.md and follow instructions."`. The daemon creates
`HEARTBEAT.md` in the workspace if it is missing. Heartbeat health is an
optional component owned by Xin.

## What a scheduled agent run is told

An `agent_session` task, a goal step, a heartbeat item and an agent cron job
each run as a single-shot turn: the system message, the earlier runs of the
run's session (keyed `xin:{task id}` or `cron:{job id}`), then the run's own
user message. An earlier run's prompt is replayed with its stored card and
date when its attachment row is found, followed by its reply; its tool
results are shown as cleared stubs.

A goal step runs in its goal's shared session, keyed `xin:goal:{goal id}`, so
a later step opens with the earlier steps of the same goal, and its earlier
tool results are kept within the replay budget rather than cleared. Steps of
one goal run strictly in sequence, so the shared session has one writer at a
time, and rows stored under the old per-step keys are not migrated. An
ownerless goal step takes an owner bucket derived from `xin:goal:{goal id}`,
so every step of one goal shares that bucket.

The scheduler opens the user message with
a task card, read from its own store and marked in its header as data and not
as an instruction. The task instruction follows the line `[Task instruction]`.
The card carries:

- the kind of run (Xin task, goal step, heartbeat item or cron job), the id and
  the name, and the schedule (interval, cron expression or one-shot instant);
- the run number: for a Xin task its stored run count plus one; for a cron job
  the length of the retained run history plus one, shown as "N or later" once
  the history has reached `[cron] max_run_history`;
- the previous run's start time, status and the last 3,000 bytes of its stored
  output, each line prefixed with `| `;
- for a goal step: the goal's name and description, "step i of n", every step
  with its status, and the outputs of the completed earlier steps (last 2,000
  bytes each, 8,000 bytes in total, newest steps first), and on a retry the
  previous attempt's output.

The card is redacted with the persistence redaction before it enters the
request. It is stored as the attachment prefix of the run's input, beside the
message and not in it, so later runs replay it with the same bytes; it is not
part of recall. The
stored Xin run output is cut at 16 KiB and the cut ends with a `...[truncated]`
line. A named agent's job still gets that agent's identity text as its system
prompt, not the full interactive prompt.

Cron `session_target` accepts `isolated` (default) and `main`. Today both take
the same path: the job runs as a single-shot turn under the session key
`cron:{job_id}` and its result stays in the run history; `main` does not
deliver anything to a main session.

Scheduled jobs and Xin tasks store a `context`: `task` (default) or `fresh`.
It is set by the cron and xin tools on `add` (cron also on `schedule`, and
inside the `update` patch; xin on `update`) and by `POST` and `PATCH`
`/api/cron/jobs`. `fresh` drops only the replay of earlier runs of the job's
or task's session; recall from memory and the task card are unchanged. Goal
steps always use their goal's shared session, whatever the task's `context`.
The Postgres cron store refuses `fresh`, so a create with `fresh` fails and
leaves no job behind.

Scheduled jobs and Xin tasks also store a `deliver`: `note` (default), `message`
or `none`. It says how the owner hears about a finished run. `note` is a one-line
notice at the owner's next interactive turn. `message` sends the reply of each
successful run as a mailbox message into the session that created the job, which
wakes a turn there. The message is from the job's scheduled session
(`xin:{task id}` or `cron:{job id}`) and starts with `[Scheduled result]`, the
job name or id, the finish time in RFC 3339 UTC, the reply after redaction (cut
at 16,000 characters with ` [truncated]`) and the transcript session. Its id is
derived from the job and the run, so a run recorded twice sends one message. An
error run sends nothing and stays a note. A `message` job with no creating
session falls back to a note: an owner-less job (operator CLI), a job whose
creating message is no longer stored, a job created through a route, a
non-SQLite backend, or a send refused by the inbound setting. A note that was
delivered as a message is not repeated at the next turn. `none` writes no notice and no result row. A
one-off agent task created from a session (the xin tool with an agent task that is
not recurring and has a `message_event_id` in the trusted scope; the cron tool
with an agent job on an `at` schedule and a `message_event_id`) defaults to `message`; everything else
defaults to `note`. An explicit `deliver` always wins. It is set by the xin tool
on `add` and `update`, by the cron tool on `add` and in the `update` patch, and
by `POST` and `PATCH` `/api/cron/jobs`. The xin job routes return it; the cron
job routes return it with the rest of the job. A route write has no creating
session, so a route job without `deliver` is a `note`. The Postgres cron store
refuses `message` and `none` at create and patch, as it refuses `fresh`; there the
cron tool's default falls back to `note`, while an explicit `message` or `none` is
refused.

## HEARTBEAT.md materialization

Each trimmed line that starts with `-` followed by a space becomes a recurring
`system` / `agent_session` Xin task. The task name is `heartbeat:` plus the
SHA-256 digest of that bullet, so reordering the file keeps the same task.
The payload is the configured prompt, a blank line, `[Heartbeat Task]`, a
space, and the bullet. The recurring interval is
`heartbeat.interval_minutes` minutes with a five-minute floor.

A new materialized task is due immediately. Prompt, interval, description, and
enablement changes update the existing row. Removed bullets are disabled, not
deleted. Re-adding the same bullet re-enables that task. A no-change
reconciliation writes no lifecycle events. `prx xin pause` is not durable for
these tasks: while the bullet remains, the next reconciliation sets the task
enabled again. Remove the bullet to keep it disabled. `prx xin cancel` still
prevents scheduling: reconciliation may restore enabled, but status stays
cancelled until `prx xin resume` resets it to pending.

Materialized tasks run only inside the configured active-hour window. When
`start` is greater than `end`, the window wraps midnight. Due tasks outside
the window wait until it opens.

## Leases and fencing

A cron claim is one immediate transaction. The authority tuple is
`claim_owner`, `attempt_id`, `claimed_at`, and `claim_expires_at`. A due claim
requires the job to be enabled, still at the snapshot `next_run` and schedule,
not terminal, and either unclaimed or past `claim_expires_at`. The scheduler
process identity is `cron-scheduler-` plus a UUID, reused across poll cycles.

Xin goal steps use `(lease_owner, lease_epoch)` as the authority token and
`lease_expires_at` as the liveness deadline. A successful claim increments
`lease_epoch`, assigns the owner, and sets the expiry. Epoch zero means the
step has never been claimed. Renewal requires the same owner and epoch and a
lease that has not expired; it extends only the expiry. Completion and failure
clear owner and expiry and keep the epoch. A later claim, including by the same
worker, increments the epoch. Checkpoint, running, complete, and fail writes
require the current owner, epoch, and a live lease. The runner heartbeat
renews that generation; a lost renewal or an elapsed deadline cancels the
in-flight execution so it cannot overwrite a newer owner. The Xin worker
identity is `prx:{pid}:{host_hash}`.

A step is claimable when it is `pending` or `stale`, or when it is `claimed` or
`running` with an expired lease. The same claim also requires that every lower
sequence in the goal is `completed`.

Legacy Xin tasks use the same owner/epoch/expiry fencing on `xin_tasks`.

## Ordering and adoption

`next_runnable_step` returns the lowest-sequence `pending` or `stale` step
whose earlier sequences are all `completed`. A prior pending, claimed,
running, stale, or failed step blocks later steps. The claim write uses the
same prerequisite, so a stale read cannot take a later step after an earlier
step changes.

Legacy-task adoption is opt-in (`xin.adopt_legacy_tasks`, default `false`) and
one immediate transaction. It accepts only an enabled, stale, non-recurring
task, creates a goal whose first step is the migrated work and whose second
step is a verification marker, records a unique `xin_task_adoptions` link,
disables the source task, and appends the matching events. A repeated adoption
returns the linked goal. Recurring tasks are not adopted; heartbeat bullets
stay recurring Xin tasks.

Adopted steps copy the source task's owner, topic, parent, source-message,
kind, and priority into the shared runtime envelope.

## Transactions

Task execution results commit in one immediate transaction: the completed or
failed result and bounded output, run and failure counters, the run-history
row, recurring reschedule state, lifecycle events, and one outbox row per
event. The task must still be `running` under the same lease; otherwise the
commit returns without changing state.

Lease-fenced step completion and failure use the same immediate boundary for
the step result, terminal event, goal-progress recompute, and outbox rows.

Xin local truth and the shared memory event spine are separate SQLite files.
There is no atomic commit across them. Each local event enqueue and its outbox
row are a savepoint pair. Delivery reuses the outbox event id with
`INSERT OR IGNORE`, so a crash after the external insert is safe to replay. A
delivery failure does not undo committed local truth. Each repository access
drains at most 100 pending outbox rows.

## Operator view

Calendar, timezone, and delayed schedules stay on `prx cron`. Xin owns
autonomous interval work and durable ordered workflows. `prx cron add`,
`add-at`, `add-every`, and `once` create shell jobs. Agent jobs are created
through the cron tool with `job_type` `agent`, or `payload.kind` `agentTurn`
or `systemEvent`.

```bash
prx cron add '0 9 * * *' 'echo good-morning'
prx cron add-at 2026-11-11T09:00:00Z 'echo once'
prx cron add-every 60000 'echo interval'
prx cron once 30m 'echo later'
prx cron update <job-id> --expression '0 8 * * *'
prx cron pause <job-id>
prx cron resume <job-id>
prx cron remove <job-id>
prx cron mark-operator <job-id>

prx xin status
prx xin list
prx xin add health-watch "Inspect service health" --recurring --interval-secs 3600
prx xin update <task-id> --priority high
prx xin run <task-id>
prx xin runs <task-id> --limit 20
prx xin pause <task-id>
prx xin resume <task-id>
prx xin cancel <task-id>

prx xin goals add release-checklist
prx xin steps add <goal-id> verify "Verify release artifacts"
prx xin goals get <goal-id>
prx xin goals cancel <goal-id>
```

Pause prevents future claims on cron jobs and ordinary Xin tasks and leaves an
active execution to finish. Cancel revokes the active task or step lease so
the runner stops that execution. For HEARTBEAT.md tasks, pause is undone by
the next reconciliation while the bullet remains; cancel keeps status
cancelled, so scheduling still skips it, until `prx xin resume`.
`prx cron mark-operator` and `prx xin mark-operator` record that a pre-existing
shell command is the operator's own, so it runs at the operator level.

A cron `shell` job and a Xin shell task run with the same child environment
as the `shell` tool at the level of the run (the configured `[autonomy] level`
for operator work, default Full): at Full every variable except the secret set
(provider keys and other credentials are absent), below Full the safe baseline
plus `[runtime] shell_env_allowlist`. A command that read a provider key from
its environment no longer finds it; ordinary variables such as `PATH` and
custom ones stay at Full. See [child process environment](security.md#child-process-environment).

Field defaults live in the [configuration reference](configuration.md):

```toml
[heartbeat]
interval_minutes = 30
active_hours = [8, 23]
prompt = "Check HEARTBEAT.md and follow instructions."

[xin]
interval_minutes = 5
stale_timeout_minutes = 60
adopt_legacy_tasks = false

[cron]
max_run_history = 50
```
