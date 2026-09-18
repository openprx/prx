#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

# Do not leave __pycache__ beside the workflow scripts.
export PYTHONDONTWRITEBYTECODE=1

python3 -m unittest discover \
    --start-directory .github/workflows/scripts \
    --top-level-directory .github/workflows/scripts \
    --pattern 'test_release_gate.py' \
    --verbose

echo "Release gate fixture tests passed."
