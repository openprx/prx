"""Exercise the workflow's actual runner selector and keep required gates intact."""

import itertools
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/ci-run.yml"


class FastciRunnerTests(unittest.TestCase):
    def test_self_hosted_jobs_never_reference_secrets(self):
        workflow = WORKFLOW.read_text()
        jobs_start = workflow.index("\njobs:\n")
        # A workflow-level env would also be inherited by self-hosted jobs.
        self.assertNotRegex(workflow[:jobs_start], r"\bsecrets\s*(?:\.|\[)")
        jobs = re.split(r"(?=^    [a-z][a-z-]*:\n)", workflow[jobs_start:], flags=re.M)
        scanned = set()
        for job in jobs:
            runs_on = re.search(r"^        runs-on: (.*)$", job, flags=re.M)
            if not runs_on:
                continue
            if any(marker in runs_on[1] for marker in ("heavy_runner", "self-hosted", "prx-qa-sandboxed")):
                name = job.split(":", 1)[0].strip()
                scanned.add(name)
                with self.subTest(job=name):
                    self.assertNotRegex(job, r"\bsecrets\s*(?:\.|\[)")
        self.assertEqual(scanned, {"lint", "lint-all-features", "architecture", "test-build", "test-shard", "bench-check", "mutants"})

    def test_runner_selection_and_timeout_for_events_forks_and_overrides(self):
        workflow = WORKFLOW.read_text()
        selector = re.search(r"HEAVY_RUNNER: \$\{\{ (.*?) \}\}", workflow)[1]
        script = workflow.split("            - name: Select heavy job runner\n", 1)[1].split("            - name: Detect docs-only", 1)[0].split("              run: |\n", 1)[1]
        script = "\n".join(line[18:] for line in script.splitlines() if line.startswith("                  "))
        for event, ref, fork, variable, manual in itertools.product(("pull_request", "push", "workflow_dispatch", "schedule"), ("refs/heads/main", "refs/heads/topic", "refs/tags/example"), (False, True), ("", "github"), ("", "github")):
            values = {"vars.PRX_CI_RUNNER": variable, "inputs.runner": manual,
                      "github.event_name": event, "github.ref": ref, "github.repository": "project/runtime",
                      "github.event.pull_request.head.repo.full_name": "fork/runtime" if fork else "project/runtime"}
            # Selector uses only comparisons, parentheses and &&/|| with string
            # operands, whose short-circuit semantics match Python's and/or.
            expression = re.sub(r"\b(?:vars|inputs|github)\.[\w.]+", lambda match: repr(values[match[0]]), selector)
            runner = eval(expression.replace("&&", " and ").replace("||", " or "), {"__builtins__": {}})
            eligible = (event == "push" and ref == "refs/heads/main") or (event == "pull_request" and not fork)
            hosted = variable == "github" or manual == "github" or not eligible
            with self.subTest(event=event, ref=ref, fork=fork, variable=variable, manual=manual):
                self.assertEqual(runner, '"ubuntu-latest"' if hosted else '["self-hosted","linux","x64","prx-qa-sandboxed"]')
                with tempfile.NamedTemporaryFile() as output:
                    result = subprocess.run(["bash", "-c", script], env={"HEAVY_RUNNER": runner, "GITHUB_OUTPUT": output.name}, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("heavy_timeout=" + ("45" if hosted else "15"), Path(output.name).read_text())


if __name__ == "__main__":
    unittest.main()
