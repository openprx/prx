# PRX 0.8.9 Release Notes

Release date: 2026-07-17

PRX 0.8.9 supersedes the undeployed 0.8.8 tag. It contains the same runtime,
persistence, ConfigGeneration, and Stage 9 convergence work, plus the migration
compatibility repair discovered by the deployment preflight.

## Migration compatibility repair

PRX 0.8.8 changed the checksum descriptor of already-published SQLite migration
version 4 and PostgreSQL migration version 5. An existing 0.8.7 ledger therefore
failed closed during `prx migrate status`. PRX 0.8.9 restores those immutable
descriptors and records the added MessageEvent execution metadata under new
versions: SQLite 15 and PostgreSQL 20.

No 0.8.8 binary was deployed. The existing production binary and databases
remain on 0.8.7 state.

## Upgrade procedure

1. Back up the active binary, configuration, database, active-workspace pointer,
   and user-service units.
2. Do not run `prx init` or `prx migrate baseline`.
3. Run the old binary's read-only `migrate status`, `verify`, and `dry-run`.
4. Run the 0.8.9 binary's read-only `migrate status`, `verify`, `dry-run`, and
   `migrate plan --target-version 15` for SQLite (or target 20 for PostgreSQL).
5. Install the 0.8.9 binary atomically, restart the PRX and wacli user services,
   and repeat status/verify plus doctor, runtime, health, and readiness checks.

The complete feature and compatibility notes remain in
`docs/release-notes-0.8.8.md`; 0.8.9 changes only the release version and
migration-history compatibility described above.
