#!/usr/bin/env python3
"""Enforce the physical-line limit on tracked first-party source files."""

import json
import os
from pathlib import Path
import subprocess
import sys

LIMIT = 5000
EXTENSIONS = frozenset('c cc cjs cpp css go h html in js jsx mjs proto ps1 py rs sh svelte tmpl ts tsx wit'.split())
BUILD_NAMES = frozenset(('Dockerfile', 'Makefile', 'Justfile', 'makefile', 'justfile', 'compose.yml', 'compose.yaml', 'docker-compose.yml', 'docker-compose.yaml'))
# The vendored Matrix SDK retains its upstream license and origin. New vendor
# trees are scanned as first-party until their provenance is explicitly added.
VENDOR_ROOTS = (('vendor', 'matrix-sdk-0.19.1'),)


def is_declared_source(path):
    name = path.name
    return (path.suffix.lower().lstrip('.') in EXTENSIONS or name in BUILD_NAMES
            or name.startswith('Dockerfile.')
            or (path.parts[:2] == ('.github', 'workflows') and path.suffix in ('.yaml', '.yml')))


def inspect(root, paths):
    root = root.resolve()
    violations = []
    checked = 0
    excluded = []
    for name in paths:
        relative = Path(name)
        if any(relative.parts[:len(vendor)] == vendor for vendor in VENDOR_ROOTS):
            excluded.append(name)
            continue
        path = root / relative
        try:
            if path.is_symlink():
                target = path.resolve(strict=True)
                if not target.is_relative_to(root):
                    raise ValueError('symlink escapes repository')
            data = path.read_bytes()
            if not (is_declared_source(relative) or data.startswith(b'#!')):
                continue
            data.decode('utf-8')
            count = data.count(b'\n') + bool(data and not data.endswith(b'\n'))
            checked += 1
            if count > LIMIT:
                violations.append({'path': name, 'lines': count, 'bytes': len(data), 'limit': LIMIT, 'reason': 'line limit exceeded'})
        except (OSError, UnicodeError, ValueError) as exc:
            violations.append({'path': name, 'reason': str(exc), 'limit': LIMIT})
    return {'checked': checked, 'excluded_vendored': len(excluded), 'violations': violations}


def main():
    root = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], text=True).strip())
    paths = subprocess.check_output(['git', '-C', os.fspath(root), 'ls-files', '-z']).decode('utf-8').split('\0')
    result = inspect(root, filter(None, paths))
    print(json.dumps(result, indent=2))
    return bool(result['violations'])


if __name__ == '__main__':
    sys.exit(main())
