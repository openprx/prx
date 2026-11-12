"""Execute the launch/finalization block from the actual embedded runner."""
import ast
from pathlib import Path
import signal
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch


SOURCE = Path(__file__).with_name('run_tb2.sh').read_text().split("<<'PY'\n", 1)[1].rsplit('\nPY', 1)[0]
TREE = ast.parse(SOURCE)
LOOP = [node for node in TREE.body if isinstance(node, ast.For)][-1]
BLOCK = next(node for node in LOOP.body if isinstance(node, ast.Try))
CODE = compile(ast.Module(body=[BLOCK], type_ignores=[]), 'run_tb2.sh', 'exec')


class RunnerTests(unittest.TestCase):
    def context(self):
        return dict(subprocess=subprocess, signal=signal, seconds=120, command=['fixture'],
                    job=SimpleNamespace(glob=lambda _: [Path('/fixture/trial/config.json')]),
                    finalize=Mock(), scan_secrets=Mock(), secret='fixture', failed=False,
                    sys=SimpleNamespace(stderr=None))

    def test_finalize_failure_still_scans_secrets(self):
        context = self.context()
        context['finalize'].side_effect = ValueError('broken record')
        proc = Mock()
        proc.wait.return_value = 0
        with patch.object(subprocess, 'Popen', return_value=proc):
            exec(CODE, context)
        self.assertTrue(context['failed'])
        context['scan_secrets'].assert_called_once_with(Path('/fixture/trial'), 'fixture')

    def test_sigint_deadline_kills_harbor_and_next_task_runs(self):
        context = self.context()
        stuck, next_task = Mock(), Mock()
        stuck.wait.side_effect = [subprocess.TimeoutExpired('fixture', 1920),
                                  subprocess.TimeoutExpired('fixture', 180), -9]
        next_task.wait.return_value = 0
        with patch.object(subprocess, 'Popen', side_effect=[stuck, next_task]) as launch:
            for _ in range(2):
                exec(CODE, context)
        self.assertEqual(launch.call_count, 2)
        stuck.send_signal.assert_called_once_with(signal.SIGINT)
        stuck.kill.assert_called_once_with()
        self.assertEqual(stuck.wait.call_args_list[-1].kwargs, {'timeout': 30})
        self.assertTrue(context['failed'])
        self.assertEqual(context['scan_secrets'].call_count, 2)
