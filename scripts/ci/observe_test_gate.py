#!/usr/bin/env python3
"""Observe a CI command without changing its arguments, result, or test gates."""

import datetime
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys


def emit(event, **fields):
    print(json.dumps({"test_gate": event, "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), **fields}), flush=True)


def resources():
    fields = {"disk_free_bytes": shutil.disk_usage(".").free, "load_average": os.getloadavg()}
    for name, path in {
        "memory": "/proc/meminfo",
        "cgroup_memory_events": "/sys/fs/cgroup/memory.events",
        "cgroup_memory_current": "/sys/fs/cgroup/memory.current",
        "cgroup_memory_max": "/sys/fs/cgroup/memory.max",
    }.items():
        try:
            fields[name] = Path(path).read_text()[:4096].strip()
        except OSError:
            pass
    emit("resources", **fields)


def run(command):
    resources()
    child = subprocess.Popen(command, start_new_session=True)
    received_signal = None

    def forward(signum, _frame):
        nonlocal received_signal
        received_signal = signum
        emit("signal", signal=signum, child_pid=child.pid)
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass

    previous = {sig: signal.signal(sig, forward) for sig in (signal.SIGTERM, signal.SIGINT)}
    emit("started", child_pid=child.pid)
    try:
        while True:
            try:
                status = child.wait(timeout=30)
                break
            except subprocess.TimeoutExpired:
                resources()
        # A wrapper termination must never become success if the child catches it.
        result = 128 + received_signal if received_signal else (128 - status if status < 0 else status)
        resources()
        emit("finished", child_status=status, exit_code=result)
        return result
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit("usage: observe_test_gate.py COMMAND [ARG ...]")
    sys.exit(run(sys.argv[1:]))
