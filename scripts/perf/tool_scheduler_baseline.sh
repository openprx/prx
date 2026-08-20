#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

start_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "[tool-scheduler-baseline] start=$start_ts"

echo "[1/3] cargo check"
cargo check

echo "[2/3] scheduler regression tests"
cargo test run_tool_call_loop_executes_read_only_tools_with_bounded_parallelism -- --nocapture
cargo test run_tool_call_loop_keeps_stateful_tools_strictly_serial -- --nocapture
cargo test read_only_calls_still_run_concurrently_without_any_rollout_gate -- --nocapture

echo "[3/3] timeout/concurrency decoupling"
cargo test parallel_and_serial_lanes_agree_that_tools_have_no_timeout -- --nocapture
cargo test no_schedule_config_can_reintroduce_a_timeout_or_a_serial_fallback -- --nocapture

end_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "[tool-scheduler-baseline] done=$end_ts"
