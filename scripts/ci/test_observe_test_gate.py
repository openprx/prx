"""The CI observer must preserve failures and forward termination to its child."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import unittest


SCRIPT = Path(__file__).with_name("observe_test_gate.py")


class ObserverTests(unittest.TestCase):
    def test_exit_status_is_preserved(self):
        for code in (0, 7, 143):
            with self.subTest(code=code):
                result = subprocess.run(
                    [sys.executable, str(SCRIPT), sys.executable, "-c", f"raise SystemExit({code})"],
                    capture_output=True, text=True, timeout=5,
                )
                self.assertEqual(result.returncode, code)
                self.assertEqual(json.loads(result.stdout.splitlines()[-1])["exit_code"], code)

    def test_termination_is_forwarded_and_is_not_success(self):
        with subprocess.Popen(
            [sys.executable, str(SCRIPT), sys.executable, "-c", "import time; time.sleep(120)"],
            stdout=subprocess.PIPE, text=True,
        ) as process:
            try:
                for line in process.stdout:
                    if json.loads(line)["test_gate"] == "started":
                        break
                os.kill(process.pid, signal.SIGTERM)
                output, _ = process.communicate(timeout=5)
                self.assertEqual(process.returncode, 143)
                finished = json.loads(output.splitlines()[-1])
                self.assertEqual(finished["child_status"], -signal.SIGTERM)
            finally:
                if process.poll() is None:
                    process.kill()


if __name__ == "__main__":
    unittest.main()
