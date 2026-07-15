# Xin lease fencing

Status: Step 8.1 implementation baseline

Xin goal steps use an explicit claim generation. The persisted tuple
`(lease_owner, lease_epoch)` is the authority token for every execution-side
mutation; `lease_expires_at` remains the liveness deadline.

## Contract

- A successful claim atomically increments `xin_steps.lease_epoch`, assigns the
  owner, and sets the expiry. Epoch zero means the step has never been claimed.
- Renewal requires the same owner and epoch plus a non-expired lease. It extends
  only the expiry; it never creates a new generation.
- The claimed-to-running transition, checkpoint write, completion, retry/fail
  transition, and their terminal lifecycle event require the same owner and
  epoch plus a non-expired lease.
- Completion or failure clears owner and expiry but preserves the epoch. A
  later claim increments it, including when the same process/worker ID reclaims
  the step.
- A stale execution cannot renew, checkpoint, complete, fail, or append a
  terminal/retry event after another claim wins.
- Terminal/retry event payloads record `step_id`, `lease_owner`, and
  `lease_epoch`. The state update and event append run in the same immediate
  SQLite transaction; an event failure rolls back the transition.

## Execution cancellation

The existing runner heartbeat retains the current epoch while updating its
expiry. `Ok(None)` from renewal or the lease deadline cancels the execution
token. The runner drops the in-flight execution future, waits for the heartbeat
task, skips checkpoint/terminal persistence, and therefore cannot overwrite a
new owner's state. Shell execution uses the shared process-group kill/reap
boundary when its future is dropped.

## Compatibility

Repository initialization adds `lease_epoch INTEGER NOT NULL DEFAULT 0` to
existing `xin_steps` tables. New tables include the column directly. Existing
unclaimed rows start at epoch zero and receive epoch one on their first claim.
The serialized `XinStep` model exposes the current epoch for inspection.

Unfenced checkpoint/complete/fail helpers are not present in production
builds. Test-only legacy helpers remain only where older state-machine fixtures
need to exercise non-runner goal rollup independently of lease execution.

Step 8.2 remains responsible for broader transactional transition cleanup; in
particular, this step does not redesign stale-sweep or multi-step goal
scheduling ownership.
