"""The CI observer must preserve failures and forward termination to its child."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import unittest


SCRIPT = Path(__file__).with_name("observe_test_gate.py")


def events_from(stdout):
    return [json.loads(line) for line in stdout.splitlines() if line]


class ObserverTests(unittest.TestCase):
    def test_exit_status_is_preserved(self):
        for code in (0, 7, 143):
            with self.subTest(code=code):
                result = subprocess.run(
                    [sys.executable, str(SCRIPT), sys.executable, "-c", f"raise SystemExit({code})"],
                    capture_output=True, text=True, timeout=5,
                )
                self.assertEqual(result.returncode, code)
                events = events_from(result.stdout)
                self.assertEqual(events[-1]["exit_code"], code)
                self.assertEqual([event["test_gate"] for event in events if event["test_gate"] == "signal"], [])

    def test_termination_is_forwarded_and_is_not_success(self):
        with subprocess.Popen(
            [sys.executable, str(SCRIPT), sys.executable, "-c", "import time; time.sleep(120)"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ) as process:
            try:
                prefix = []
                for line in process.stdout:
                    event = json.loads(line)
                    prefix.append(event)
                    if event["test_gate"] == "started":
                        break
                os.kill(process.pid, signal.SIGTERM)
                output, err = process.communicate(timeout=5)
                self.assertEqual(process.returncode, 143)
                self.assertEqual(err, "")
                events = prefix + events_from(output)
                started = next(event for event in events if event["test_gate"] == "started")
                signal_events = [event for event in events if event["test_gate"] == "signal"]
                self.assertEqual(len(signal_events), 1)
                self.assertEqual(signal_events[0]["signal"], int(signal.SIGTERM))
                self.assertEqual(signal_events[0]["child_pid"], started["child_pid"])
                finished = events[-1]
                self.assertEqual(finished["test_gate"], "finished")
                self.assertEqual(finished["child_status"], -signal.SIGTERM)
                self.assertEqual(finished["exit_code"], 143)
            finally:
                if process.poll() is None:
                    process.kill()


if __name__ == "__main__":
    unittest.main()
