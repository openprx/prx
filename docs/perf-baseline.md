# OpenPRX Performance Baseline

This document records OpenPRX Pure-mode 100-turn baseline performance numbers
as the first v0.4.0 reference.

## Measurement method

- Test entry: `tests/chat_perf_baseline.rs::s5_release_p0_2_pure_perf_baseline`
- Provider: `MockEnvProvider` (test-mock feature) + `OPENPRX_MOCK_RESPONSE=mock8byt`
- Metrics:
- **M1 chunk→snapshot**: time from `next().await` for each stream chunk through
  to finished consumption
- **M2 end-to-end-turn**: total time of one complete stream call (stream
  construction plus every chunk)
- **M3 peak RSS delta**: difference in Linux `/proc/self/status` `VmHWM` before
  and after 100 turns
- Tools: no extra crates (criterion/wiremock/mockall are banned)
- Threshold rule: v0.4.0 uses a sanity ceiling only; v0.4.1+ compares against
  this baseline with a 2x deviation rule

## v0.4.0 baseline (N=100 turns)

| Metric | p50 | p95 | p99 |
|------|-----|-----|-----|
| chunk→snapshot | 50ns | 60ns | 80ns |
| end-to-end-turn | 481ns | 541ns | 4.709µs |

**RSS** (Linux only):

- starting VmHWM: 8292 KB
- ending VmHWM: 9396 KB
- delta: 1104 KB

## Sanity Ceilings (v0.4.0)

- `p99 chunk→snapshot < 50ms` — mock provider, no network; exceeding this means
  the dispatcher or reducer has unexpected synchronous blocking
- `p99 end-to-end-turn < 500ms` — 100 turns with no network should be well below
  this
- `peak_rss_delta < 100MB` — growth of 100MB or more after 100 turns indicates a
  likely leak

## v0.4.1+ comparison rule

Later versions treat this baseline as the reference. A deviation of more than 2x
on p99 or RSS delta is a regression and needs a cause or a fix in the PR notes.
