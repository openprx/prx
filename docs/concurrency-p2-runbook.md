# Concurrency P2 Runbook and On-Call Handbook

Updated: 2026-03-13

## Config Knobs

Under `[agent]`:

- `parallel_tools`
- `concurrency_rollout_stage`
- `concurrency_rollout_sample_percent`
- `concurrency_rollout_channels`
- `concurrency_kill_switch_force_serial`
- `concurrency_auto_rollback_enabled`
- rollback thresholds:
  - `concurrency_rollback_timeout_rate_threshold`
  - `concurrency_rollback_cancel_rate_threshold`
  - `concurrency_rollback_error_rate_threshold`

## Start / Stop / Reload

```bash
systemctl --user restart openprx
openprx config-reload
```

## Staged Rollout Procedure

1. Set `parallel_tools=true`.
2. Start with `concurrency_rollout_stage="stage_a"`.
3. Keep `concurrency_rollout_sample_percent=0` (stage default) unless manual override is needed.
4. Observe SLO windows before moving to `stage_b` -> `stage_c` -> `full`.

## Emergency Rollback

Priority order:

1. Set `concurrency_kill_switch_force_serial=true` and reload config.
2. If instability persists, set `parallel_tools=false` and reload config.
3. Keep a log snapshot of `tool batch execution` fields for postmortem.

## Drill Path (Required)

1. Enable `stage_a` rollout.
2. Force timeout scenario (test or staging).
3. Verify rollback/degrade counters increment.
4. Trigger kill switch and verify serial behavior.
5. Revert kill switch and confirm staged rollout resumes.

## Incident Triage Checklist

1. Confirm current rollout stage and kill switch state.
2. Check timeout/cancel/degrade/rollback counters.
3. Correlate with provider/tool backend health.
4. Apply kill switch if SLO critical threshold is hit.
5. Record timeline, config diffs, and recovery evidence.
