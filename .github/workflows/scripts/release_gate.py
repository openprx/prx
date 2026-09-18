#!/usr/bin/env python3
"""Exact-SHA release gate.

Decides whether the commit behind a release tag is allowed to publish, by
reading the GitHub Actions API for that exact SHA and inspecting the required
workflows job by job.

Design constraints:

- The gate never inspects the Release workflow itself, so it cannot deadlock on
  the run that is asking the question.
- Duplicate runs for one SHA are resolved by taking the newest run per required
  workflow. A newer run that has not finished keeps the decision pending
  instead of letting an older green run answer for it.
- Job-level conclusions decide, not the run conclusion. A docs-only fast-path
  run is green at run level while the Rust gates are skipped, and skipping a
  required job is a gate failure.
- Every non-success state fails closed with a named reason.

Offline mode (``--bundle``) reads a recorded API payload instead of the network
so the decision logic can be unit-tested against fixtures.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any

PASS = "pass"
FAIL = "fail"
PENDING = "pending"

# Run/job states that may still turn into a success later.
PENDING_STATUSES = frozenset({"queued", "in_progress", "requested", "waiting", "pending"})
# Conclusions that are terminal failures wherever they appear in a required run.
FAILING_CONCLUSIONS = frozenset({"failure", "timed_out", "cancelled", "action_required", "stale"})


@dataclass(frozen=True)
class RequiredWorkflow:
    """A workflow that must be green on the exact release SHA.

    ``jobs`` are matched by the display name GitHub reports for the job. Matrix
    legs carry their parameters in the name, so they are listed individually.
    """

    name: str
    jobs: tuple[str, ...]


REQUIRED_WORKFLOWS: tuple[RequiredWorkflow, ...] = (
    RequiredWorkflow(
        name="CI Run",
        jobs=(
            "Installer and Toolchain Contracts",
            "Windows Installer Contract",
            "Detect Change Scope",
            "Lint Gate (Format + Clippy + Strict Delta)",
            "Test",
            "Build (Smoke)",
            "Windows Check",
            "CI Required Gate",
        ),
    ),
    RequiredWorkflow(
        name="Sec Audit",
        jobs=(
            "Security Audit",
            "License & Supply Chain (advisories)",
            "License & Supply Chain (bans licenses sources)",
        ),
    ),
)


@dataclass
class WorkflowDecision:
    """Outcome for one required workflow."""

    workflow: str
    state: str
    run_id: int | None = None
    run_attempt: int | None = None
    url: str = ""
    reasons: list[str] = field(default_factory=list)

    @property
    def run_label(self) -> str:
        if self.run_id is None:
            return "-"
        return f"{self.run_id} (attempt {self.run_attempt or 1})"

    @property
    def detail(self) -> str:
        return "; ".join(self.reasons) if self.reasons else "all required jobs succeeded"


@dataclass
class GateResult:
    """Aggregate decision over every required workflow."""

    sha: str
    decisions: list[WorkflowDecision]

    @property
    def state(self) -> str:
        states = {decision.state for decision in self.decisions}
        if FAIL in states:
            return FAIL
        if PENDING in states:
            return PENDING
        return PASS


def newest_run(runs: list[dict[str, Any]]) -> dict[str, Any] | None:
    """Return the newest run, so duplicate runs for one SHA are unambiguous."""

    def sort_key(run: dict[str, Any]) -> tuple[str, int, int]:
        started = run.get("run_started_at") or run.get("created_at") or ""
        return (str(started), int(run.get("run_attempt") or 1), int(run.get("id") or 0))

    if not runs:
        return None
    return max(runs, key=sort_key)


def index_jobs(jobs: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    """Index jobs by display name, keeping the highest job id per name."""
    indexed: dict[str, dict[str, Any]] = {}
    for job in jobs:
        name = str(job.get("name") or "")
        previous = indexed.get(name)
        if previous is None or int(job.get("id") or 0) >= int(previous.get("id") or 0):
            indexed[name] = job
    return indexed


def evaluate_jobs(required: RequiredWorkflow, jobs: list[dict[str, Any]]) -> tuple[str, list[str]]:
    """Check required jobs plus any failing job inside the same run."""
    indexed = index_jobs(jobs)
    reasons: list[str] = []
    pending = False
    failed = False

    for job_name in required.jobs:
        job = indexed.get(job_name)
        if job is None:
            failed = True
            reasons.append(f"required job '{job_name}' is absent from the run")
            continue
        status = str(job.get("status") or "")
        conclusion = str(job.get("conclusion") or "")
        if status in PENDING_STATUSES:
            pending = True
            reasons.append(f"required job '{job_name}' is {status}")
            continue
        if conclusion == "skipped":
            failed = True
            reasons.append(
                f"required job '{job_name}' was skipped "
                "(a docs-only fast-path run does not satisfy the release gate)"
            )
            continue
        if conclusion != "success":
            failed = True
            reasons.append(f"required job '{job_name}' concluded '{conclusion or 'unknown'}'")

    for job in jobs:
        conclusion = str(job.get("conclusion") or "")
        if conclusion in FAILING_CONCLUSIONS and str(job.get("name") or "") not in required.jobs:
            failed = True
            reasons.append(f"job '{job.get('name')}' concluded '{conclusion}'")

    if failed:
        return FAIL, reasons
    if pending:
        return PENDING, reasons
    return PASS, reasons


def evaluate_workflow(
    required: RequiredWorkflow,
    sha: str,
    runs: list[dict[str, Any]],
    jobs_by_run: dict[str, list[dict[str, Any]]],
) -> WorkflowDecision:
    """Decide one required workflow for the exact SHA."""
    candidates = [
        run
        for run in runs
        if str(run.get("name") or "") == required.name
        and str(run.get("head_sha") or "").lower() == sha.lower()
    ]
    if not candidates:
        return WorkflowDecision(
            workflow=required.name,
            state=PENDING,
            reasons=[f"no '{required.name}' run recorded for {sha}"],
        )

    run = newest_run(candidates) or {}
    decision = WorkflowDecision(
        workflow=required.name,
        state=PENDING,
        run_id=int(run.get("id") or 0) or None,
        run_attempt=int(run.get("run_attempt") or 1),
        url=str(run.get("html_url") or ""),
    )

    status = str(run.get("status") or "")
    if status in PENDING_STATUSES:
        decision.reasons.append(f"newest run is {status}")
        return decision

    conclusion = str(run.get("conclusion") or "")
    jobs = jobs_by_run.get(str(decision.run_id))
    if jobs is None:
        decision.state = FAIL
        decision.reasons.append("job data for the newest run is unavailable")
        return decision

    job_state, job_reasons = evaluate_jobs(required, jobs)
    decision.reasons.extend(job_reasons)

    if conclusion == "cancelled":
        decision.state = FAIL
        decision.reasons.insert(0, "newest run was cancelled")
        return decision
    if conclusion != "success":
        decision.state = FAIL
        decision.reasons.insert(0, f"newest run concluded '{conclusion or 'unknown'}'")
        return decision

    decision.state = job_state
    return decision


def evaluate(
    bundle: dict[str, Any],
    required_workflows: tuple[RequiredWorkflow, ...] = REQUIRED_WORKFLOWS,
) -> GateResult:
    """Evaluate every required workflow against one recorded API bundle."""
    sha = str(bundle.get("sha") or "")
    runs = list(bundle.get("runs") or [])
    jobs_by_run = {str(key): list(value) for key, value in (bundle.get("jobs") or {}).items()}
    decisions = [evaluate_workflow(item, sha, runs, jobs_by_run) for item in required_workflows]
    return GateResult(sha=sha, decisions=decisions)


def finalize(result: GateResult) -> GateResult:
    """Turn any remaining pending decision into a failure (fail closed)."""
    for decision in result.decisions:
        if decision.state == PENDING:
            decision.state = FAIL
            decision.reasons.append("still not conclusive when the gate budget expired")
    return result


def render_table(result: GateResult) -> str:
    """Render the decision as a fixed-width table."""
    header = ("Workflow", "Run", "State", "Detail")
    rows = [
        (decision.workflow, decision.run_label, decision.state.upper(), decision.detail)
        for decision in result.decisions
    ]
    widths = [
        max([len(header[index])] + [len(row[index]) for row in rows])
        for index in range(3)
    ]
    template = f"{{:<{widths[0]}}}  {{:<{widths[1]}}}  {{:<{widths[2]}}}  {{}}"
    lines = [
        template.format(*header),
        template.format(*("-" * width for width in (*widths, len(header[3])))),
    ]
    for row in rows:
        lines.append(template.format(*row))
    lines.append("")
    lines.append(f"Release gate decision for {result.sha}: {result.state.upper()}")
    return "\n".join(lines)


def api_get(url: str, token: str) -> dict[str, Any]:
    """Perform one authenticated GitHub API GET."""
    request = urllib.request.Request(url)
    request.add_header("Accept", "application/vnd.github+json")
    request.add_header("X-GitHub-Api-Version", "2022-11-28")
    request.add_header("User-Agent", "prx-release-gate")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    # The URL is always the configured GitHub API host.
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.loads(response.read().decode("utf-8"))


def api_collect(url: str, token: str, key: str, max_pages: int = 10) -> list[dict[str, Any]]:
    """Collect a paginated GitHub API list."""
    items: list[dict[str, Any]] = []
    for page in range(1, max_pages + 1):
        separator = "&" if "?" in url else "?"
        payload = api_get(f"{url}{separator}per_page=100&page={page}", token)
        chunk = list(payload.get(key) or [])
        items.extend(chunk)
        if len(chunk) < 100 or len(items) >= int(payload.get("total_count") or 0):
            break
    return items


def fetch_bundle(repo: str, sha: str, token: str, api_url: str) -> dict[str, Any]:
    """Fetch runs and jobs for one SHA into the offline bundle shape."""
    quoted_sha = urllib.parse.quote(sha, safe="")
    runs = api_collect(
        f"{api_url}/repos/{repo}/actions/runs?head_sha={quoted_sha}", token, "workflow_runs"
    )
    required_names = {item.name for item in REQUIRED_WORKFLOWS}
    jobs: dict[str, list[dict[str, Any]]] = {}
    for run in runs:
        if str(run.get("name") or "") not in required_names:
            continue
        run_id = str(run.get("id") or "")
        if not run_id or run_id in jobs:
            continue
        jobs[run_id] = api_collect(
            f"{api_url}/repos/{repo}/actions/runs/{run_id}/jobs", token, "jobs"
        )
    return {"sha": sha, "runs": runs, "jobs": jobs}


def load_bundle(path: str) -> dict[str, Any]:
    """Load a recorded bundle from disk."""
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Gate release publication on the exact tag SHA.")
    parser.add_argument("--sha", help="Commit SHA behind the release tag.")
    parser.add_argument(
        "--repo", default=os.environ.get("GITHUB_REPOSITORY", ""), help="OWNER/NAME."
    )
    parser.add_argument("--bundle", help="Recorded API payload; disables all network access.")
    parser.add_argument(
        "--wait-seconds",
        type=int,
        default=0,
        help="Bounded wait budget for still-running required workflows (online mode only).",
    )
    parser.add_argument(
        "--poll-seconds", type=int, default=30, help="Poll interval during the wait."
    )
    parser.add_argument("--summary", help="Append the decision table to this file as well.")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)

    if args.bundle:
        bundle = load_bundle(args.bundle)
        sha = args.sha or str(bundle.get("sha") or "")
        bundle["sha"] = sha
        result = finalize(evaluate(bundle))
    else:
        if not args.sha or not args.repo:
            print("release gate: --sha and --repo are required in online mode", file=sys.stderr)
            return 2
        token = os.environ.get("GITHUB_TOKEN", "")
        api_url = os.environ.get("GITHUB_API_URL", "https://api.github.com").rstrip("/")
        deadline = time.monotonic() + max(0, args.wait_seconds)
        while True:
            try:
                bundle = fetch_bundle(args.repo, args.sha, token, api_url)
            except (urllib.error.URLError, OSError, ValueError) as error:
                print(f"release gate: GitHub API read failed: {error}", file=sys.stderr)
                return 2
            result = evaluate(bundle)
            if result.state != PENDING or time.monotonic() >= deadline:
                break
            print(render_table(result))
            print(f"Waiting {args.poll_seconds}s for required workflows to finish...", flush=True)
            time.sleep(max(1, args.poll_seconds))
        result = finalize(result)

    table = render_table(result)
    print(table)
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as handle:
            handle.write("## Exact-SHA release gate\n\n```\n")
            handle.write(table)
            handle.write("\n```\n")
    if result.state != PASS:
        sys.stdout.flush()
        print(
            "release gate: publication blocked. Re-run or fix the named workflows on this exact "
            "SHA, then re-run the release workflow.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
