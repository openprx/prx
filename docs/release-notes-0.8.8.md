# PRX 0.8.8 Release Notes

Release date: 2026-07-16

## Overview

PRX 0.8.8 completes the architecture-convergence sequence across process
ownership, durable events, tool execution, configuration generations, runtime
capability domains, and production diagnostics. The release preserves the
existing `agent::loop_`, `MessageEvent`, Cron, and Xin ownership models while
removing private execution, persistence, and reload paths around them.

## Runtime and persistence changes

- Chat and agent share the authoritative turn owner, tool execution service,
  terminal commit, approval, MessageEvent, and usage/cost settlement paths.
- MessageEvent idempotency is scoped by workspace in SQLite and PostgreSQL.
- Tool execution uses reserve/execute/commit/replay semantics and will not
  execute the same idempotency key twice across supported entry points.
- Process workers, Cron claims, Xin transitions, webhook ingestion, and service
  readiness now report durable and operating-system truth.
- Nodes, Skills, Plugins/hooks, media artifacts, and provider routing/cost are
  owned by bounded, long-lived runtime components rather than request-local
  private state.

## Configuration generations

One process-level `ConfigGenerationManager` owns configuration publication.
Every admitted turn, message, Cron/Xin job, webhook, and runtime event pins a
typed generation. Reloads are serialized and classified as:

- `snapshot_hot`: visible to newly admitted work without restarting unrelated
  components;
- `rebuild_and_swap`: the candidate is prepared and published only after
  readiness succeeds;
- `supervisor_restart`: the affected generation-owned component is replaced
  with fencing, rollback, and no overlap;
- `process_restart_only`: desired state advances while active state remains on
  the previous value and the response reports `restart_required`.

Failed preparation, readiness, or commit keeps the previous active generation.
In-flight work never changes generation mid-operation.

## Upgrade and migration procedure

1. Back up and hash the active configuration, database, current PRX binary, and
   user-service units.
2. Do **not** run `prx init` against the active configuration.
3. Before replacement, run the read-only schema checks for the active config:

   ```bash
   prx migrate status
   prx migrate verify
   prx migrate dry-run
   prx migrate plan
   ```

4. Review the SQLite/PostgreSQL MessageEvent and configuration-generation
   migration plan. Use `migrate baseline` only when intentionally adopting an
   existing schema after backup; it is not a routine upgrade command.
5. Install the release binary atomically, then restart the PRX and wacli user
   services together.
6. Verify `prx doctor`, `prx doctor runtime`, schema status, health/readiness,
   active configuration source, and bounded post-restart logs.

Schema changes add typed configuration-generation lineage and replace global
MessageEvent idempotency uniqueness with a workspace-scoped constraint. Keep
the old binary, configuration snapshot, and database backup until runtime
acceptance and the observation window pass.

## Compatibility notes

- Provider/model/tools and other process-only configuration fields can return
  `restart_required`; this is an intentional truthful result, not a reload
  failure.
- Existing in-flight work completes on its pinned generation while new work is
  admitted on the new active generation.
- Unsupported hybrid-memory merge remains fail-closed rather than silently
  discarding an unmergeable draft.
- The active daemon is a user service. Use `systemctl --user` when checking or
  restarting `prx.service` and `wacli-sync.service`.

## Required acceptance

The release is not production-accepted until the exact release SHA passes the
full locked Rust gate, live PostgreSQL conformance, configuration supervisor
fault tests, atomic deployment checks, deployed Chat/TUI acceptance in tmux
`demo`, reload/rollback, dual-daemon fencing, process-kill truth, wacli ingress,
and the configured observation window.
