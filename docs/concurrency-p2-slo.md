# Concurrency P2 SLO

Updated: 2026-03-13

## Scope

This SLO applies to the tool scheduler path in `run_tool_call_loop` with staged rollout controls and automatic rollback thresholds.

## Key Metrics

- `openprx_tool_batches_total`
- `openprx_tool_timeouts_total`
- `openprx_tool_cancellations_total`
- `openprx_tool_degrades_total`
- `openprx_tool_rollbacks_total`

Structured log event: `tool batch execution` with fields:

- `rollout_stage`
- `batch_size`
- `concurrency_window`
- `timeout_count`
- `cancel_count`
- `error_count`
- `degraded`
- `rollback`
- `rollback_reason`
- `kill_switch_applied`

## SLI Definitions

- Timeout rate = `tool_timeouts / total_tool_calls_in_batch_windows`
- Cancel rate = `tool_cancellations / total_tool_calls_in_batch_windows`
- Degrade rate = `tool_degrades / tool_batches`
- Rollback rate = `tool_rollbacks / tool_batches`

## SLO Targets

- Timeout rate: <= 5% (15m rolling window)
- Cancel rate: <= 3% (15m rolling window)
- Rollback rate: <= 1% (60m rolling window)
- Degrade rate: <= 3% (60m rolling window)

## Alert Thresholds

- Warning:
  - timeout rate > 8% for 10m
  - cancel rate > 5% for 10m
- Critical:
  - rollback rate > 3% for 10m
  - 3+ rollbacks in 15m

## Operational Judgment Checklist

1. Confirm `rollout_stage` and `kill_switch_applied` from logs.
2. Compare timeout/cancel spikes against upstream provider or tool backend incidents.
3. If critical threshold is hit, set `concurrency_kill_switch_force_serial=true` and reload config.
4. After stabilization for one full SLO window, re-enable staged rollout from `stage_a`.
