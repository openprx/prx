"""Gate infrastructure changes must exercise the Rust gate they control."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("detect_change_scope.sh").resolve()


class ChangeScopeTests(unittest.TestCase):
    def classify(self, changed_path):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True).strip()

            git("init", "--quiet")
            git("config", "user.email", "fixture@example.invalid")
            git("config", "user.name", "CI Fixture")
            (root / "baseline").write_text("baseline\n")
            git("add", "baseline")
            git("commit", "--quiet", "-m", "baseline")
            base = git("rev-parse", "HEAD")
            changed = root / changed_path
            changed.parent.mkdir(parents=True, exist_ok=True)
            changed.write_text("changed\n")
            git("add", changed_path)
            git("commit", "--quiet", "-m", "change")
            output = root / "scope-output"
            subprocess.run(
                ["bash", str(SCRIPT)], cwd=root, check=True,
                env={**os.environ, "GITHUB_OUTPUT": str(output), "EVENT_NAME": "push", "BASE_SHA": base},
            )
            return output.read_text().splitlines()

    def test_gate_changes_cannot_take_the_non_rust_fast_path(self):
        for path, workflow in (
            (".github/workflows/ci-run.yml", "true"),
            ("scripts/ci/test_home_guard.sh", "false"),
            ("scripts/ci/observe_test_gate.py", "false"),
            ("scripts/ci/detect_change_scope.sh", "false"),
        ):
            with self.subTest(path=path):
                lines = self.classify(path)
                self.assertIn("rust_changed=true", lines)
                self.assertIn("docs_only=false", lines)
                self.assertIn("workflow_changed=" + workflow, lines)

    def test_docs_only_changes_keep_the_docs_fast_path(self):
        lines = self.classify("docs/example.md")
        self.assertIn("rust_changed=false", lines)
        self.assertIn("docs_only=true", lines)
        self.assertIn("docs_changed=true", lines)


if __name__ == "__main__":
    unittest.main()
