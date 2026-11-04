"""Persistent runners must only reset a guard home in disposable space."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from guard_home_path import guard_home_path


class GuardHomePathTests(unittest.TestCase):
    def test_default_override_and_unsafe_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            scratch, workspace, home = (root / name for name in ("temp", "workspace", "real-home"))
            env = {"RUNNER_TEMP": str(scratch), "GITHUB_WORKSPACE": str(workspace), "HOME": str(home)}
            self.assertEqual(guard_home_path(env), scratch / "home")
            self.assertEqual(guard_home_path({**env, "PRX_GUARD_HOME": str(workspace / "guard")}), workspace / "guard")
            for unsafe in (root, scratch, workspace, home, scratch / ".." / "outside"):
                with self.subTest(path=unsafe), self.assertRaises(ValueError):
                    guard_home_path({**env, "PRX_GUARD_HOME": str(unsafe)})
            scratch.mkdir()
            (scratch / "escape").symlink_to(home, target_is_directory=True)
            with self.assertRaises(ValueError):
                guard_home_path({**env, "PRX_GUARD_HOME": str(scratch / "escape" / "guard")})
            with self.assertRaises(ValueError):
                guard_home_path({**env, "PRX_GUARD_HOME": str(scratch / "home"), "HOME": str(scratch / "home" / "real")})
            with self.assertRaises(ValueError):
                guard_home_path({"HOME": str(home)})

    def test_stale_home_is_reset_and_failure_cleans_only_guard_directory(self):
        script = Path(__file__).with_name("test_home_guard.sh").resolve()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            home, scratch, binaries = (root / name for name in ("real-home", "temp", "bin"))
            for path in (home, scratch, binaries):
                path.mkdir()
            (home / "keep").write_text("persistent credentials placeholder")
            sibling = scratch / "home.keep"
            sibling.write_text("keep sibling")
            # Exercise guard setup and EXIT trap without Cargo or a network sandbox.
            uname = binaries / "uname"
            uname.write_text("#!/bin/sh\necho UnsupportedGuardTestPlatform\n")
            uname.chmod(0o755)
            for _ in range(2):
                guard = scratch / "home"
                guard.mkdir()
                (guard / "stale").write_text("old job")
                result = subprocess.run(["bash", str(script), "/usr/bin/true"], capture_output=True, text=True,
                    env={**os.environ, "HOME": str(home), "RUNNER_TEMP": str(scratch),
                         "PRX_GUARD_HOME": str(guard), "PRX_GUARD_SUBCOMMAND": "exec",
                         "PATH": str(binaries) + os.pathsep + os.environ["PATH"]})
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertFalse(guard.exists())
                self.assertEqual(sorted(path.name for path in scratch.iterdir()), ["home.keep"])
                self.assertEqual(sibling.read_text(), "keep sibling")
                self.assertEqual([path.name for path in home.iterdir()], ["keep"])


if __name__ == "__main__":
    unittest.main()
