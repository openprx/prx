# Concurrency P2 Pressure and Fault Baseline

Updated: 2026-03-13

## Minimal Baseline Runner

Run:

```bash
bash scripts/perf/concurrency_p2_baseline.sh
```

The script executes:

- deterministic scheduler tests for parallel/read-only and serial/stateful lanes
- rollback-path scheduler regression test
- `cargo check` as compile baseline guard

## Fault Injection Matrix

1. Timeout storm
- Setup: reduce `read_only_tool_timeout_secs` or use slow tool fixtures.
- Expectation: timeout count increases, rollback may trigger when threshold exceeded.

2. Cancellation burst
- Setup: cancel active channel/session tasks using existing cancellation path.
- Expectation: cancellation count increases and no late history writes.

3. Tool backend failure burst
- Setup: return tool errors from read-only tool fixtures.
- Expectation: error count increases, auto-degrade to serial if threshold exceeded.

4. Kill switch drill
- Setup: set `concurrency_kill_switch_force_serial=true` and run hot reload.
- Expectation: new turns skip read-only batching and remain serial.

## Regression Steps

1. Run baseline script.
2. Execute one fault scenario.
3. Verify metrics/log fields changed as expected.
4. Toggle rollback/kill-switch off and confirm recovery.
5. Re-run baseline script.

## Pass Criteria

- `cargo check` passes.
- Scheduler tests pass.
- Fault injection produces expected counter/log changes.
- Kill switch is reversible through config reload.
