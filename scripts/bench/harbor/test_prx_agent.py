"""Exercise adapter calls without installing Harbor in the unittest environment."""
import asyncio
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import time
from pathlib import Path
import sqlite3
import sys
import tempfile
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

from record import AGENT_KEYS, unavailable


base = ModuleType('harbor.agents.installed.base')
base.BaseInstalledAgent = object
spec = importlib.util.spec_from_file_location('prx_agent_under_test', Path(__file__).with_name('prx_agent.py'))
adapter = importlib.util.module_from_spec(spec)
with patch.dict(sys.modules, {'harbor.agents.installed.base': base}):
    spec.loader.exec_module(adapter)


class FakeEnvironment:
    def __init__(self, mode):
        self.calls = []
        self.mode = mode
        self.uploads = []

    async def exec(self, **kwargs):
        self.calls.append(kwargs)
        command = kwargs['command']
        if 'agent -t' in command:
            if self.mode == 'timeout':
                raise asyncio.TimeoutError()
            if self.mode == 'runtime_timeout':
                raise RuntimeError('Command timed out after 30 seconds')
        if command == 'command -v timeout' and self.mode == 'no_timeout':
            return SimpleNamespace(return_code=1)
        return SimpleNamespace(return_code=0)

    async def download_file(self, remote, local):
        if self.mode == 'download_timeout':
            raise asyncio.TimeoutError()
        if self.mode == 'sqlite_error':
            raise sqlite3.OperationalError('fixture interrupted')
        Path(local).write_bytes(b'invalid database')


class AdapterTests(unittest.IsolatedAsyncioTestCase):
    async def test_exec_calls_never_contain_secret_and_cleanup_always_runs(self):
        secret = 'prx-fixture-secret-not-exported'
        for mode in ('normal', 'timeout', 'runtime_timeout', 'no_timeout', 'download_timeout', 'sqlite_error'):
            with self.subTest(mode=mode):
                agent = object.__new__(adapter.PrxAgent)
                agent.key_file = Path('/opt/ops/bench/secrets/fixture')
                agent.model_name = 'prx/model'
                agent.provider_override = 'custom:http://host.containers.internal:1234/v1'
                agent.provider = agent.provider_override
                agent.model = 'model'
                agent.prices = dict.fromkeys(('input', 'output', 'cache_read', 'cache_write'), 0)
                agent.temperature = 0
                agent.git_sha = 'sha'
                agent.binary_sha256 = 'binary'
                agent.agent_timeout_sec = 120
                agent.version = lambda: 'prx fixture'
                agent._metadata = {k: unavailable('not produced') for k in AGENT_KEYS}
                environment = FakeEnvironment(mode)

                async def upload(environment, **kwargs):
                    environment.uploads.append(kwargs)

                agent._upload_config_text = upload
                with patch.object(adapter, 'read_key_file', return_value=secret):
                    await agent.run('fixture instruction', environment, None)
                calls = json.dumps(environment.calls)
                self.assertNotIn(secret, calls)
                self.assertNotIn(secret[:8], calls)
                self.assertNotIn('env', {k for call in environment.calls for k in call})
                self.assertIn(secret, environment.uploads[0]['content'])
                self.assertEqual(environment.calls[-1]['command'], 'rm -rf /opt/prx-bench')
                commands = [call['command'] for call in environment.calls]
                self.assertIn(adapter.KILL_PRX, commands)
                self.assertEqual(agent._metadata['per_attempt_models']['status'], 'unavailable')
                invocation = next(c for c in commands if 'agent -t' in c)
                self.assertIn('NO_PROXY=host.containers.internal', invocation)
                if mode == 'no_timeout':
                    self.assertNotIn('timeout -k', invocation)
                else:
                    self.assertIn('timeout -k 5s 70s', invocation)
                execution = next(c for c in environment.calls if 'agent -t' in c['command'])
                self.assertEqual(execution['timeout_sec'], 70 if mode == 'no_timeout' else 85)
                self.assertEqual(agent._metadata['agent_exec_timeout_sec'], 70)
                self.assertTrue(all(c['timeout_sec'] == 10 for c in environment.calls if c['command'] == adapter.KILL_PRX))

    async def test_key_file_rejects_wrong_path_and_mode(self):
        with self.assertRaises(ValueError):
            adapter.read_key_file('/tmp/fixture')
        with tempfile.TemporaryDirectory() as directory:
            key = Path(directory) / 'key'
            key.write_text('fixture-credential')
            # Mock only path validation; fstat still checks the real descriptor.
            with patch.object(adapter, 'Path', side_effect=lambda p: key if str(p).endswith('/key') else key.parent):
                key.chmod(0o644)
                with self.assertRaises(ValueError):
                    adapter.read_key_file('/opt/ops/bench/secrets/key')
                key.chmod(0o600)
                self.assertEqual(adapter.read_key_file('/opt/ops/bench/secrets/key'), 'fixture-credential')


class DescendantCleanupTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith('linux'), 'requires Linux /proc and setsid')
    def test_cleanup_kills_separate_session_descendants(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / 'prx-stand-in'
            shutil.copyfile(shutil.which('sh'), executable)
            executable.chmod(0o700)
            marker, ready = root / 'marker', root / 'ready'
            child = 'echo $$ > "$2"; while :; do echo x >> "$1"; sleep 0.1; done'
            parent = subprocess.Popen([str(executable), '-c',
                'setsid sh -c "$1" sh "$2" "$3" & wait', 'sh', child, str(marker), str(ready)])
            child_pid = None
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if ready.exists() and marker.exists():
                        child_pid = int(ready.read_text())
                        if marker.stat().st_size >= 4:
                            break
                    time.sleep(0.02)
                self.assertIsNotNone(child_pid, 'stand-in child did not start')
                self.assertGreaterEqual(marker.stat().st_size, 4, 'marker must grow before cleanup')
                subprocess.run(['sh', '-c', adapter.KILL_PRX],
                    env={**os.environ, 'PRX_EXE': str(executable)}, timeout=10, check=True)
                parent.wait(timeout=2)
                size = marker.stat().st_size
                # This is the specified one-second post-cleanup observation window.
                time.sleep(1)
                self.assertEqual(marker.stat().st_size, size, 'detached tool child survived cleanup')
                stat_path = Path(f'/proc/{child_pid}/stat')
                if stat_path.exists():
                    self.assertEqual(stat_path.read_text().rsplit(') ', 1)[1].split()[0], 'Z',
                                     'detached child must be dead (an unreaped zombie is allowed)')
            finally:
                if child_pid is not None:
                    try:
                        os.killpg(child_pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                parent.kill()
                parent.wait(timeout=2)
