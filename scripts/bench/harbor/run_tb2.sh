#!/usr/bin/env bash
# QA host only. Positional arguments are names in the pinned TB2 dataset.
set -euo pipefail
unset PRX_BENCH_API_KEY
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$script_dir/../../.."
export DOCKER_HOST=unix:///run/user/1000/podman/podman.sock
export PODMAN_COMPOSE_PROVIDER=/opt/ops/bench/bin/docker-compose
export PODMAN_COMPOSE_WARNING_LOGS=false
export PYTHONPATH="$PWD/scripts/bench/harbor${PYTHONPATH:+:$PYTHONPATH}"
exec /opt/ops/bench/venv/bin/python - "$@" <<'PY'
import argparse
import math
import os
from pathlib import Path
import subprocess
import signal
import sys
import time
import tomllib
from profile import read_bounded
from prx_agent import read_key_file
from record import finalize, scan_secrets, sha256, write, unavailable

p = argparse.ArgumentParser()
p.add_argument('--model', required=True)
p.add_argument('--key-file', required=True, type=Path)
p.add_argument('--binary', required=True, type=Path)
p.add_argument('--git-sha', required=True)
p.add_argument('--agent-timeout-multiplier', type=float, default=1)
p.add_argument('--temperature', type=float, default=0)
p.add_argument('--provider')
for key in ('input', 'output', 'cache_read', 'cache_write'):
    p.add_argument('--price-' + key.replace('_', '-'), type=float, required=True)
p.add_argument('tasks', nargs='+')
a = p.parse_args()
if not math.isfinite(a.agent_timeout_multiplier) or a.agent_timeout_multiplier <= 0:
    p.error('timeout multiplier must be finite and positive')
secret = read_key_file(a.key_file)
sha_file = a.binary.with_name(a.binary.name + '.sha256')
digest = read_bounded(sha_file).decode().split()[0]
dataset = Path('/opt/ops/bench/datasets/terminal-bench-2.0')
manifest = dataset / 'prx-dataset.json'
if not manifest.is_file():
    if not any(dataset.glob('*/task.toml')) and not any(dataset.glob('terminal-bench/*/task.toml')):
        subprocess.run(['/opt/ops/bench/venv/bin/harbor', 'download', 'terminal-bench@2.0',
                        '-o', str(dataset)], timeout=900, check=True)
    # Harbor 0.25 export adds a dataset-name directory. Normalize it once.
    nested = dataset / 'terminal-bench'
    if nested.is_dir():
        for child in nested.iterdir():
            child.rename(dataset / child.name)
        nested.rmdir()
    entries = {}
    for task_file in dataset.glob('*/task.toml'):
        if len(entries) >= 256:
            raise ValueError('dataset task bound exceeded')
        entries[task_file.parent.name] = sha256(read_bounded(task_file))
    if not entries:
        raise ValueError('download did not produce local TB2 tasks')
    write(manifest, {'dataset_tag': 'terminal-bench@2.0', 'task_toml_sha256': entries,
                     'commit': unavailable('Harbor local export does not retain git metadata')})
failed = False
for index, task_name in enumerate(a.tasks):
    if Path(task_name).name != task_name or task_name in ('.', '..'):
        p.error('tasks must be dataset directory names')
    task = (dataset / task_name).resolve(strict=True)
    if task.parent != dataset:
        p.error('task escaped pinned dataset directory')
    config = tomllib.loads(read_bounded(task / 'task.toml').decode())
    seconds = float(config['agent']['timeout_sec']) * a.agent_timeout_multiplier
    if not math.isfinite(seconds) or seconds <= 50:
        p.error('effective task timeout must exceed 50 seconds')
    deadline = time.monotonic() + 240 * 60
    while float(read_bounded('/proc/loadavg', 1024).decode().split()[0]) >= 8:
        if time.monotonic() >= deadline:
            raise SystemExit(75)
        time.sleep(30)
    job = Path('/opt/ops/bench/jobs') / (time.strftime('%Y-%m-%d__%H-%M-%S', time.gmtime()) + f'-prx-{index}')
    command = ['/opt/ops/bench/venv/bin/harbor', 'run', '-p', str(task), '-a', 'prx_agent:PrxAgent',
               '-m', a.model, '-n', '1', '-o', str(job.parent), '--job-name', job.name,
               '--agent-timeout-multiplier', str(a.agent_timeout_multiplier)]
    options = {'binary_path': str(a.binary.resolve()), 'binary_sha256': digest, 'git_sha': a.git_sha,
               'key_file': str(a.key_file), 'agent_timeout_sec': seconds, 'temperature': a.temperature}
    options.update({'price_' + key: getattr(a, 'price_' + key) for key in ('input', 'output', 'cache_read', 'cache_write')})
    if a.provider:
        options['provider'] = a.provider
    for key, value in options.items():
        command += ['--ak', f'{key}={value}']
    try:
        proc = subprocess.Popen(command)
        try:
            rc = proc.wait(timeout=seconds + 1800)
        except subprocess.TimeoutExpired:
            proc.send_signal(signal.SIGINT)
            try:
                rc = proc.wait(timeout=180)
            except subprocess.TimeoutExpired:
                proc.kill()
                rc = proc.wait(timeout=30)
                failed = True
        failed |= rc != 0
    finally:
        trials = list(job.glob('*/config.json'))
        if len(trials) > 16:
            raise ValueError('too many trials for a serial single-task job')
        for cfg in trials:
            try:
                finalize(cfg.parent)
            except (ValueError, OSError, KeyError, TypeError) as error:
                failed = True
                print(f"finalize failed for {cfg.parent.name}: {type(error).__name__}", file=sys.stderr)
            finally:
                scan_secrets(cfg.parent, secret)
raise SystemExit(1 if failed else 0)
PY
