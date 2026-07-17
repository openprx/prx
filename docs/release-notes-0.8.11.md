# PRX 0.8.11 Release Notes

Release date: 2026-07-17

PRX 0.8.11 superseded the undeployed 0.8.8, 0.8.9, and 0.8.10 tags. It contains
the runtime, persistence, ConfigGeneration, Stage 9, migration-history, and
SQLite/PostgreSQL Cron legacy-schema repairs from those candidates.

## Windows release repair

The 0.8.10 release workflow exposed a Windows-only compile error in the OpenRC
script renderer: a pure shell-quoting helper was compiled only on Unix even
though the renderer is type-checked on Windows. The helper is now available on
all targets. The release workflow also creates a GitHub Release only after all
required platform builds succeed, so a platform failure cannot publish a
partial asset set.

The incomplete 0.8.10 GitHub Release was removed, its immutable tag was retained,
and no 0.8.10 binary was deployed. The complete 0.8.11 Release was published,
but Stage 5 found a PostgreSQL Cron panic when a synchronous client was queried
inside the deployed Tokio runtime. The deployment was atomically rolled back to
0.8.7. Use 0.8.12 instead.

## Upgrade procedure

1. Back up the active binary, configuration, databases, active-workspace
   pointer, and user-service units.
2. Do not run `prx init` or `prx migrate baseline`.
3. Run the old binary's read-only migration checks.
4. Run the exact release binary's read-only migration checks against an isolated
   copy of the deployed workspace, then exercise `cron list` on that copy.
5. Install the exact audited release binary atomically, restart PRX and wacli,
   and complete the Stage 5 and Stage 6 acceptance matrices.

The complete feature notes remain in `docs/release-notes-0.8.8.md`; the later
notes document each stopped release candidate and its repair.
