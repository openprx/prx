# Xin ordered prerequisites and adoption

Status: Step 8.3 implementation baseline

Xin goals now enforce their sequence at both the scheduler read boundary and
the lease-authority write boundary. Legacy-task adoption is one idempotent
transaction, and goal lineage reaches the shared Agent runtime envelope.

## Ordered execution contract

`next_runnable_step` returns a pending or stale step only when every lower
sequence in the same goal is `completed`. A prior pending, claimed, running,
stale, or failed step blocks all later steps.

The same `NOT EXISTS` prerequisite is part of the atomic lease claim. A caller
cannot bypass ordering by obtaining a later step ID directly, and a stale read
cannot win a claim after an earlier step changes state. Existing owner/epoch/
expiry fencing remains unchanged.

## Idempotent legacy adoption

`xin_task_adoptions` records one unique legacy-task-to-goal link. Adoption runs
under `BEGIN IMMEDIATE` and atomically:

- verifies that the source task is enabled, stale, and non-recurring;
- creates one goal with the source owner, topic, parent, source-message, kind,
  priority, description, payload, execution mode, retries, and approval grant;
- creates the migrated work as sequence 1 and the verification marker as
  sequence 2;
- inserts the durable adoption link;
- disables the source legacy task; and
- appends the goal-created and task-adopted events plus their durable outbox
  rows.

A repeated direct adoption returns the linked goal without creating another
goal or event. Startup scans skip the disabled source and report zero new
adoptions. Any goal, link, disable, event, or outbox failure rolls back the
whole adoption.

The older one-shot migration helper remains only for compatibility tests. The
canonical goal/add-step APIs now use immediate transactions; adoption uses the
same internal goal insertion primitive.

## Runtime lineage

The step bridge now copies the owning goal's owner, topic, source-message,
parent, kind, and priority instead of synthesizing anonymous defaults. Xin then
builds a shared `RuntimeEnvelope` carrying owner, topic, step task ID, and
source-message ID.

The Agent entrypoint accepts that caller-owned envelope for scheduler-driven
single-shot turns. Its existing public CLI entry keeps the old default envelope.
This makes Xin lineage available to memory principals/write scope, document
ingest, tool approval scope, audit, message events, and terminal settlement
without introducing a second runtime context model.

## Boundaries

- Adoption remains opt-in through `xin.adopt_legacy_tasks`; the default is
  unchanged.
- Recurring legacy Xin tasks are not adopted. Step 8.4 owns their Heartbeat
  materialization and duplicate-loop retirement.
- This step does not push, deploy, install, or activate a runtime.
