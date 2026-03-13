#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

start_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "[concurrency-p2-baseline] start=$start_ts"

echo "[1/3] cargo check"
cargo check

echo "[2/3] scheduler regression tests"
cargo test run_tool_call_loop_executes_read_only_tools_with_bounded_parallelism -- --nocapture
cargo test run_tool_call_loop_keeps_stateful_tools_strictly_serial -- --nocapture
cargo test execute_tools_with_policy_triggers_rollback_and_forces_remaining_serial -- --nocapture

echo "[3/3] config persistence smoke"
cargo test agent_config_default_concurrency_rollout_is_off -- --nocapture

end_ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "[concurrency-p2-baseline] done=$end_ts"
