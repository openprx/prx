# PRX 0.8.10 Release Notes

Release date: 2026-07-17

PRX 0.8.10 supersedes the undeployed 0.8.8 and 0.8.9 tags. It contains the
runtime, persistence, ConfigGeneration, Stage 9, and migration-history repairs
from those candidates, plus the Cron legacy-schema upgrade fix discovered by
the first controlled 0.8.9 deployment attempt.

## Cron legacy-schema upgrade repair

The 0.8.9 scheduler initialized the attempt-identity index before adding its
`attempt_id` column to a pre-existing `cron_runs` table. Fresh databases were
unaffected, but an existing 0.8.7 workspace failed scheduler health during
startup. PRX 0.8.10 adds both legacy columns first, then creates the index. A
regression test upgrades an old populated schema and proves that the columns,
index, and historical run remain intact.

No 0.8.8 or 0.8.9 binary remained deployed. The failed 0.8.9 attempt was
atomically rolled back, and production returned to the healthy 0.8.7 binary and
workspace snapshot before this candidate was prepared.

## Upgrade procedure

1. Back up the active binary, configuration, databases, active-workspace
   pointer, and user-service units.
2. Do not run `prx init` or `prx migrate baseline`.
3. Run the old binary's read-only migration checks.
4. Run the 0.8.10 binary's read-only migration checks against an isolated copy
   of the deployed workspace, then exercise `cron list` on that copy.
5. Install the exact audited 0.8.10 binary atomically, restart PRX and wacli,
   and complete the Stage 5 and Stage 6 acceptance matrices.

The complete feature notes remain in `docs/release-notes-0.8.8.md`; the 0.8.9
and 0.8.10 notes document the two release-candidate compatibility repairs.
