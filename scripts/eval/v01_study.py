#!/usr/bin/env python3
"""Offline intent preparation and descriptive reproduction, never authority.

The Rust owner plans, admits, executes, validates and signs studies. This tool
does not execute models, verify owner MACs, create budgets, or mint eligibility.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path

MAX_EXPORT_BYTES = 256 * 1024 * 68
MAX_DRAFT_BYTES = 128 * 1024


def read_json(path, limit):
    with Path(path).open("rb") as stream:
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("input exceeds its bounded metadata limit")
    return raw, json.loads(raw)


def pinned_export(path, expected):
    raw, envelope = read_json(path, MAX_EXPORT_BYTES)
    actual = hashlib.sha256(raw).hexdigest()
    if len(expected) != 64 or expected != actual:
        raise ValueError("export does not match the trusted exact input-file pin")
    value = envelope["value"]
    report = value["report"]
    if report["schema_version"] != 1 or report["analysis"]["qualification"]:
        raise ValueError("input is not a nonqualifying V01 export")
    # A caller hash authenticates only caller-selected bytes. MAC verification
    # remains in the Rust owner; this function deliberately claims none.
    return report, value["evidence_sha256"], actual


def prepare(args):
    _, draft = read_json(args.draft, MAX_DRAFT_BYTES)
    roots = draft["roots"]
    repetitions = draft["repetitions"]
    pairs = sum(len(root["cases"]) for root in roots) * repetitions
    if not 1 <= repetitions <= 5 or not 1 <= pairs <= 32:
        raise ValueError("study exceeds the unchanged 64-stage/32-pair contract")
    protocol = draft["protocol"]
    if protocol["phase"] == "confirmatory":
        if not args.calibration_export or not args.calibration_sha256:
            raise ValueError("confirmatory preparation needs an exact-pinned calibration export")
        report, evidence, _ = pinned_export(args.calibration_export, args.calibration_sha256)
        if report["manifest"]["protocol"]["phase"] != "calibration" or not report["analysis"]["mechanism_complete"]:
            raise ValueError("calibration is not reported complete by its source")
        calibration = protocol.get("calibration") or {}
        for key in ("sample_and_uncertainty_rationale", "independent_root_sampling_basis"):
            if not str(calibration.get(key, "")).strip():
                raise ValueError("reviewed calibration/sample rationale must be supplied; no defaults are invented")
        calibration.update({
            "study": {"id": report["manifest"]["id"], "sha256": report["experiment_sha256"]},
            "report_sha256": evidence,
            # The owner derives/validates these again at plan/admission.
            "excluded_roots": [{
                "root_task_id": root["root_task_id"],
                "prompt_sha256": [hashlib.sha256(" ".join(case["prompt"].split()).lower().encode()).hexdigest() for case in root["cases"]],
            } for root in report["manifest"]["roots"]],
        })
        protocol["calibration"] = calibration
    elif protocol["phase"] != "calibration" or protocol.get("calibration") is not None:
        raise ValueError("invalid study phase/calibration intent")
    encoded = (json.dumps(draft, ensure_ascii=False, indent=2) + "\n").encode()
    if len(encoded) > MAX_DRAFT_BYTES:
        raise ValueError("prepared intent exceeds bounded source size")
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(encoded)
    return {"prepared_intent_sha256": hashlib.sha256(encoded).hexdigest(), "admitted": False,
            "next": "Use the crate study plan/admit commands; this file is not authority."}


def analyze(args):
    report, evidence, raw_hash = pinned_export(args.export, args.sha256)
    manifest = report["manifest"]
    metric = manifest["protocol"]["primary_metric"]
    rows = {(row["input"]["pair_id"], row["input"]["arm"]): row for row in report["rows"]}
    if len(rows) != len(manifest["pairs"]) * 2:
        raise ValueError("duplicate or omitted paired row")

    def score(pair, arm):
        row = rows[(pair["pair_id"], arm)]
        receipt = row.get("receipt")
        if row["state"] != "settled" or not receipt or not receipt["measured_and_valid"]:
            return 0.0
        return float(receipt["success"]) if metric == "success" else receipt["quality_ppm"] / 1_000_000

    differences = []
    for root in manifest["roots"]:
        pairs = [pair for pair in manifest["pairs"] if pair["root_task_id"] == root["root_task_id"]]
        if not pairs:
            raise ValueError("root cluster has no frozen pairs")
        differences.append(sum(score(pair, "h2") - score(pair, "baseline") for pair in pairs) / len(pairs))
    if not differences:
        raise ValueError("no independent root tasks")
    difference = sum(differences) / len(differences)
    alpha = manifest["protocol"]["alpha_ppm"] / 1_000_000
    width = math.sqrt(2 * math.log(2 / alpha) / len(differences))
    if not math.isclose(difference, report["analysis"]["difference"], abs_tol=1e-12):
        raise ValueError("descriptive reproduction differs from the input report")
    return {
        "input_file_sha256": raw_hash, "source_evidence_pin": evidence,
        "owner_verification": "not performed by this analyzer; use the crate owner report/export",
        "authority": "none", "qualification": False,
        "statistical_unit": "independent root task; within-root variants/repetitions averaged",
        "independent_roots": len(differences), "declared_pairs": len(manifest["pairs"]),
        "descriptive_difference": difference,
        "preregistered_interval_reproduction": [max(-1, difference - width), min(1, difference + width)],
        "missing_or_invalid_row_policy": "zero score within full frozen denominator; never silently drop",
        "benefit_decision": "not minted or upgraded by external analysis",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    prep = commands.add_parser("prepare")
    prep.add_argument("--draft", required=True)
    prep.add_argument("--output", required=True)
    prep.add_argument("--calibration-export")
    prep.add_argument("--calibration-sha256")
    analysis = commands.add_parser("analyze")
    analysis.add_argument("--export", required=True)
    analysis.add_argument("--sha256", required=True)
    args = parser.parse_args()
    try:
        result = prepare(args) if args.command == "prepare" else analyze(args)
    except (KeyError, ValueError, TypeError, OSError, ZeroDivisionError) as error:
        parser.exit(2, f"V01 metadata rejected: {type(error).__name__}\n")
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
