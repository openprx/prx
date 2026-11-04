"""Resolve a disposable guard home strictly inside CI scratch space."""

import os
from pathlib import Path
import sys


def guard_home_path(environ):
    roots = [Path(environ[key]).resolve() for key in ("RUNNER_TEMP", "GITHUB_WORKSPACE") if environ.get(key)]
    requested = environ.get("PRX_GUARD_HOME")
    if not requested:
        if not environ.get("RUNNER_TEMP"):
            raise ValueError("set RUNNER_TEMP or PRX_GUARD_HOME inside GITHUB_WORKSPACE")
        requested = str(Path(environ["RUNNER_TEMP"]) / "home")
    path = Path(requested).resolve()
    real_home = Path(environ["HOME"]).resolve()
    if not any(root in path.parents for root in roots):
        raise ValueError("PRX_GUARD_HOME must be strictly inside RUNNER_TEMP or GITHUB_WORKSPACE")
    if path == real_home or path in real_home.parents:
        raise ValueError("PRX_GUARD_HOME must not contain the real HOME")
    return path


if __name__ == "__main__":
    try:
        print(guard_home_path(os.environ))
    except ValueError as error:
        sys.exit(f"test_home_guard: {error}")
