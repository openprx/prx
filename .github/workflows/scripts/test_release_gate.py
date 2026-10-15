#!/usr/bin/env python3
"""Fixture tests for the exact-SHA release gate.

Run from the repository root:

    python3 -m unittest discover -s .github/workflows/scripts -p 'test_*.py' -v

or through ``scripts/ci/release_gate_tests.sh``.
"""

from __future__ import annotations

import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parents[2]
FIXTURE_DIR = SCRIPT_DIR / "release_gate_fixtures"

_spec = importlib.util.spec_from_file_location("release_gate", SCRIPT_DIR / "release_gate.py")
assert _spec is not None and _spec.loader is not None
release_gate = importlib.util.module_from_spec(_spec)
sys.modules["release_gate"] = release_gate
_spec.loader.exec_module(release_gate)


def decide(fixture: str):
    """Evaluate one fixture the way the workflow step does."""
    bundle = json.loads((FIXTURE_DIR / fixture).read_text(encoding="utf-8"))
    return release_gate.evaluate(bundle)


def reasons(result, workflow: str) -> str:
    for decision in result.decisions:
        if decision.workflow == workflow:
            return "; ".join(decision.reasons)
    raise AssertionError(f"no decision for workflow {workflow}")


class ReleaseGateDecisionTest(unittest.TestCase):
    def test_all_required_workflows_green_passes(self) -> None:
        result = decide("pass.json")
        self.assertEqual(release_gate.PASS, result.state)
        self.assertEqual(
            ["CI Run", "Sec Audit"], [decision.workflow for decision in result.decisions]
        )
        for decision in result.decisions:
            self.assertEqual(release_gate.PASS, decision.state)
            self.assertEqual([], decision.reasons)

    def test_failed_required_gate_blocks_publication(self) -> None:
        result = decide("failed.json")
        self.assertEqual(release_gate.FAIL, result.state)
        detail = reasons(result, "CI Run")
        self.assertIn("newest run concluded 'failure'", detail)
        self.assertIn("'CI Required Gate' concluded 'failure'", detail)
        self.assertIn("'Docs Quality' concluded 'failure'", detail)
        self.assertEqual(release_gate.PASS, result.decisions[1].state)

    def test_running_required_workflow_is_pending_then_fails_closed(self) -> None:
        result = decide("running.json")
        self.assertEqual(release_gate.PENDING, result.state)
        self.assertIn("newest run is in_progress", reasons(result, "CI Run"))
        finalized = release_gate.finalize(result)
        self.assertEqual(release_gate.FAIL, finalized.state)
        self.assertIn("gate budget expired", reasons(finalized, "CI Run"))

    def test_cancelled_run_blocks_publication(self) -> None:
        result = decide("cancelled.json")
        self.assertEqual(release_gate.FAIL, result.state)
        self.assertIn("newest run was cancelled", reasons(result, "CI Run"))

    def test_docs_only_green_run_does_not_satisfy_the_gate(self) -> None:
        result = decide("docs_only_green.json")
        self.assertEqual(release_gate.FAIL, result.state)
        detail = reasons(result, "CI Run")
        skipped_jobs = (
            "Lint Gate (Format + Clippy + Strict Delta)",
            "Test",
            "Build (Smoke)",
            "Windows Check",
        )
        for job in skipped_jobs:
            self.assertIn(f"required job '{job}' was skipped", detail)
        self.assertIn("docs-only fast-path", detail)

    def test_missing_security_run_fails_closed(self) -> None:
        result = decide("missing_security.json")
        self.assertEqual(release_gate.PENDING, result.state)
        self.assertIn("no 'Sec Audit' run recorded", reasons(result, "Sec Audit"))
        finalized = release_gate.finalize(result)
        self.assertEqual(release_gate.FAIL, finalized.state)
        self.assertEqual(release_gate.PASS, finalized.decisions[0].state)

    def test_newest_duplicate_run_decides_not_the_green_one(self) -> None:
        result = decide("duplicate_newest_docs_only.json")
        self.assertEqual(release_gate.FAIL, result.state)
        decision = result.decisions[0]
        self.assertEqual(35308882509, decision.run_id)
        self.assertIn("was skipped", "; ".join(decision.reasons))

    def test_newest_duplicate_run_still_running_keeps_gate_pending(self) -> None:
        result = decide("duplicate_newest_running.json")
        self.assertEqual(release_gate.PENDING, result.state)
        decision = result.decisions[0]
        self.assertEqual(35308882510, decision.run_id)
        self.assertIn("newest run is in_progress", "; ".join(decision.reasons))

    def test_release_workflow_is_not_a_gate_input(self) -> None:
        names = {workflow.name for workflow in release_gate.REQUIRED_WORKFLOWS}
        self.assertNotIn("Release", names)
        self.assertEqual({"CI Run", "Sec Audit"}, names)

    def test_other_sha_runs_are_ignored(self) -> None:
        bundle = json.loads((FIXTURE_DIR / "pass.json").read_text(encoding="utf-8"))
        for run in bundle["runs"]:
            run["head_sha"] = "0" * 40
        result = release_gate.evaluate(bundle)
        self.assertEqual(release_gate.PENDING, result.state)
        self.assertIn("no 'CI Run' run recorded", reasons(result, "CI Run"))

    def test_main_exit_codes_match_the_decision(self) -> None:
        self.assertEqual(0, release_gate.main(["--bundle", str(FIXTURE_DIR / "pass.json")]))
        self.assertEqual(1, release_gate.main(["--bundle", str(FIXTURE_DIR / "failed.json")]))
        self.assertEqual(1, release_gate.main(["--bundle", str(FIXTURE_DIR / "running.json")]))
        self.assertEqual(
            1, release_gate.main(["--bundle", str(FIXTURE_DIR / "docs_only_green.json")])
        )


class ReleaseTagBindingTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.repo = pathlib.Path(self.temp_dir.name)
        self.git("init", "--quiet")
        self.git("config", "user.name", "PRXAgent")
        self.git("config", "user.email", "prx@example.com")
        (self.repo / "release.txt").write_text("release\n", encoding="utf-8")
        self.git("add", "release.txt")
        self.git("commit", "--quiet", "-m", "test: release fixture")
        self.sha = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("tag", "-a", "v1.2.3", "-m", "prx v1.2.3")

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def git(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", *args],
            cwd=self.repo,
            check=True,
            capture_output=True,
            text=True,
        )

    def test_annotated_tag_resolving_to_gated_sha_passes(self) -> None:
        self.assertEqual(
            self.sha,
            release_gate.verify_release_tag("v1.2.3", self.sha, str(self.repo)),
        )

    def test_tag_resolving_to_another_commit_is_rejected(self) -> None:
        (self.repo / "release.txt").write_text("later\n", encoding="utf-8")
        self.git("commit", "--quiet", "-am", "test: later fixture")
        other_sha = self.git("rev-parse", "HEAD").stdout.strip()
        with self.assertRaisesRegex(release_gate.ReleaseTagError, "but the gated SHA is"):
            release_gate.verify_release_tag("v1.2.3", other_sha, str(self.repo))

    def test_untrusted_tag_syntax_is_rejected_before_git_resolution(self) -> None:
        with self.assertRaisesRegex(release_gate.ReleaseTagError, "not a valid"):
            release_gate.verify_release_tag("v1.2.3:refs/heads/main", self.sha, str(self.repo))

    def test_release_workflow_passes_the_tag_to_the_exact_sha_gate(self) -> None:
        workflow = (REPO_ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        gate_job = workflow.split("\n  validate:", 1)[0]
        self.assertIn("fetch-depth: 0", gate_job)
        self.assertIn("--sha \"$GITHUB_SHA\"", gate_job)
        self.assertIn("--tag \"$RELEASE_TAG\"", gate_job)


class ReleaseGatePolicyTest(unittest.TestCase):
    """Guard the required-job names against workflow drift."""

    def load_yaml(self):
        try:
            import yaml  # noqa: PLC0415 - optional test-only dependency
        except ImportError:  # pragma: no cover - environment without PyYAML
            self.skipTest("PyYAML is not installed")
        return yaml

    def load_workflow(self, relative: str) -> dict:
        yaml = self.load_yaml()
        return yaml.safe_load((REPO_ROOT / relative).read_text(encoding="utf-8"))

    def assert_policy_matches(self, relative: str) -> None:
        document = self.load_workflow(relative)
        policy = next(
            item for item in release_gate.REQUIRED_WORKFLOWS if item.name == document.get("name")
        )
        declared_names = {
            str(job.get("name") or job_id) for job_id, job in document["jobs"].items()
        }
        for required_job in policy.jobs:
            matched = required_job in declared_names or any(
                required_job.startswith(f"{name} (") for name in declared_names
            )
            self.assertTrue(
                matched,
                f"required job '{required_job}' has no counterpart in "
                f"{relative}: {sorted(declared_names)}",
            )

    def test_ci_required_jobs_exist_in_ci_workflow(self) -> None:
        self.assert_policy_matches(".github/workflows/ci-run.yml")

    def test_security_required_jobs_exist_in_sec_audit_workflow(self) -> None:
        self.assert_policy_matches(".github/workflows/sec-audit.yml")

    def test_release_workflow_depends_on_the_gate(self) -> None:
        jobs = self.load_workflow(".github/workflows/release.yml")["jobs"]
        self.assertIn("release-gate", list(jobs))
        for job_id in ("validate", "build", "release"):
            needs = jobs[job_id].get("needs")
            needs = [needs] if isinstance(needs, str) else list(needs or [])
            self.assertIn("release-gate", needs, f"job '{job_id}' must depend on release-gate")
        permissions = jobs["release-gate"].get("permissions") or {}
        self.assertEqual("read", permissions.get("actions"))


if __name__ == "__main__":
    unittest.main()
