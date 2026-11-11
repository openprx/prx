#!/usr/bin/env python3
"""Fail when tracked markdown contains CJK Unified Ideographs.

The product locale file is excluded explicitly so the console can keep a
Chinese translation. Expand EXPLICIT_EXCLUDES only for other product locales.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

CJK_RE = re.compile(r"[\u4e00-\u9fff]")
MD_SUFFIXES = frozenset({".md", ".mdx"})
EXPLICIT_EXCLUDES = frozenset(
    {
        "console/src/lib/i18n/zh.json",
    }
)


def should_scan(relative: str) -> bool:
    if relative in EXPLICIT_EXCLUDES:
        return False
    return Path(relative).suffix.lower() in MD_SUFFIXES


def inspect(root: Path, paths: list[str]) -> dict:
    root = root.resolve()
    violations: list[dict] = []
    checked = 0
    for name in paths:
        if not should_scan(name):
            continue
        path = root / name
        checked += 1
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeError) as exc:
            violations.append({"path": name, "reason": str(exc)})
            continue
        hits = []
        for line_no, line in enumerate(text.splitlines(), 1):
            if CJK_RE.search(line):
                hits.append({"line": line_no, "text": line.strip()[:120]})
        if hits:
            violations.append({"path": name, "hits": hits})
    return {"checked": checked, "violations": violations}


def main() -> int:
    root = Path(
        subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip()
    )
    raw = subprocess.check_output(["git", "-C", os.fspath(root), "ls-files", "-z"]).decode(
        "utf-8"
    )
    paths = [p for p in raw.split("\0") if p]
    result = inspect(root, paths)
    if not result["violations"]:
        print(
            f"CJK gate: {result['checked']} markdown file(s) clean "
            f"(excluded {', '.join(sorted(EXPLICIT_EXCLUDES))})."
        )
        return 0
    print("CJK in tracked markdown (blocking):")
    for item in result["violations"]:
        path = item["path"]
        if "hits" not in item:
            print(f"  - {path}: {item['reason']}")
            continue
        for hit in item["hits"]:
            print(f"  - {path}:{hit['line']} {hit['text']}")
    print(f"Blocking CJK hits: {sum(len(v.get('hits', [1])) for v in result['violations'])}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
