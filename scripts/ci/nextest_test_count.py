#!/usr/bin/env python3
"""Count the tests in `cargo nextest list --message-format json` output.

Prints `total=` (tests a default run executes: matched and not ignored),
`architecture=` (those in the architecture_boundaries binary except the
`retired_feature::` module, which needs the crate and runs in a shard; the
rest run in the architecture job from a standalone optimized binary,
scripts/ci/build_architecture_boundaries.sh) and `sharded=` (total minus
architecture, what the shards run).
"""
import json
import sys

ARCHITECTURE_BINARY = "architecture_boundaries"


def count(path):
    with open(path, encoding="utf-8") as handle:
        suites = json.load(handle)["rust-suites"]
    total = architecture = 0
    for suite in suites.values():
        runnable = sum(
            1
            for case in suite["testcases"].values()
            if not case["ignored"] and case["filter-match"]["status"] == "matches"
        )
        total += runnable
        if suite.get("binary-name") == ARCHITECTURE_BINARY:
            architecture += sum(
                1
                for name, case in suite["testcases"].items()
                if not case["ignored"]
                and case["filter-match"]["status"] == "matches"
                and not name.startswith("retired_feature::")
            )
    return total, architecture, total - architecture


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: nextest_test_count.py LIST.json")
    total, architecture, sharded = count(sys.argv[1])
    print(f"total={total}")
    print(f"architecture={architecture}")
    print(f"sharded={sharded}")
