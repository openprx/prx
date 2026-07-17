# PRX 0.8.12 Release Notes

Release date: 2026-07-17

PRX 0.8.12 supersedes the undeployed 0.8.8 through 0.8.11 candidates and
retains their runtime, persistence, ConfigGeneration, Stage 9, migration, and
legacy Cron compatibility work.

## PostgreSQL Cron runtime repair

The 0.8.11 Stage 5 deployment check reproduced a panic when the synchronous
PostgreSQL Cron client executed on a Tokio runtime thread. PostgreSQL Cron
operations and client shutdown now run on threads without an entered Tokio
runtime. The live PostgreSQL lifecycle regression also queries the store from
inside a Tokio runtime, matching the deployed CLI and daemon call context.

## Release integrity

All five platform builds are required. Missing build artifacts fail their job,
and the release job verifies the complete set of 10 archives and 10 checksum
files before publishing. Normal Rust CI now includes a Windows MSVC check so
Windows compile failures are found before tagging.

## Upgrade procedure

1. Back up the active binary, configuration, databases, active-workspace
   pointer, and user-service units.
2. Do not run `prx init` or `prx migrate baseline`.
3. Run read-only migration status, verification, dry-run, and plan checks.
4. Exercise SQLite and PostgreSQL legacy Cron upgrades on isolated populated
   copies using the exact certified 0.8.12 binary.
5. Install atomically, restart PRX and wacli, run the Stage 5/6 matrices, perform
   the rollback drill, redeploy, and complete the 60-minute observation window.
