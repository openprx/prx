"""Exercise adapter calls without installing Harbor in the unittest environment."""
import asyncio
import functools
import importlib.util
import json
import os
import shutil
import signal
import socket
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


def deadline(seconds):
    """Fail an async test that hangs instead of waiting for the CI job timeout."""
    def wrap(test):
        @functools.wraps(test)
        async def bounded(self):
            await asyncio.wait_for(test(self), timeout=seconds)
        return bounded
    return wrap


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
        for mode in ('normal', 'timeout', 'runtime_timeout', 'download_timeout', 'sqlite_error'):
            with self.subTest(mode=mode):
                agent = object.__new__(adapter.PrxAgent)
                agent.key_file = Path('/opt/ops/bench/secrets/fixture')
                agent.model_name = 'prx/model'
                agent.provider_override = 'custom:http://host.containers.internal:1234/v1'
                agent.provider = agent.provider_override
                agent.proxy_bind = None
                agent.proxy_peers = None
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
                self.assertNotIn(secret, environment.uploads[0]['content'])
                self.assertNotIn(secret[:8], environment.uploads[0]['content'])
                self.assertEqual(environment.calls[-1]['command'], 'rm -rf /opt/prx-bench')
                commands = [call['command'] for call in environment.calls]
                self.assertIn(adapter.KILL_PRX, commands)
                self.assertEqual(agent._metadata['per_attempt_models']['status'], 'unavailable')
                invocation = next(c for c in commands if 'agent -t' in c)
                self.assertIn('NO_PROXY=host.containers.internal', invocation)
                self.assertNotIn('timeout -k', invocation)
                self.assertNotIn('command -v timeout', commands)
                execution = next(c for c in environment.calls if 'agent -t' in c['command'])
                self.assertEqual(execution['timeout_sec'], 70)
                self.assertEqual(agent._metadata['agent_exec_timeout_sec'], 70)
                self.assertTrue(all(c['timeout_sec'] == 10 for c in environment.calls if c['command'] == adapter.KILL_PRX))

    def proxied_agent(self):
        agent = object.__new__(adapter.PrxAgent)
        agent.key_file = Path('/opt/ops/bench/secrets/fixture')
        agent.model_name = 'prx/model'
        agent.provider_override = 'custom:https://api.example.test/v1'
        agent.provider = agent.provider_override
        agent.proxy_bind = '127.0.0.1'
        agent.proxy_peers = '127.0.0.0/8'
        agent.model = 'model'
        agent.prices = dict.fromkeys(('input', 'output', 'cache_read', 'cache_write'), 0)
        agent.temperature = 0
        agent.git_sha = 'sha'
        agent.binary_sha256 = 'binary'
        agent.agent_timeout_sec = 120
        agent.version = lambda: 'prx fixture'
        agent._metadata = {k: unavailable('not produced') for k in AGENT_KEYS}

        async def upload(environment, **kwargs):
            environment.uploads.append(kwargs)

        agent._upload_config_text = upload
        return agent

    @deadline(60)
    async def test_https_provider_runs_behind_per_trial_proxy_that_dies_with_the_trial(self):
        secret = 'prx-fixture-secret-not-exported'
        hashes = set()
        for mode in ('normal', 'timeout', 'runtime_timeout', 'download_timeout'):
            with self.subTest(mode=mode):
                agent = self.proxied_agent()
                environment = FakeEnvironment(mode)
                reachable = []
                plain_exec = environment.exec

                async def exec_and_probe(plain_exec=plain_exec, environment=environment, reachable=reachable, **kwargs):
                    if 'agent -t' in kwargs['command']:
                        port = int(environment.uploads[0]['content'].split('host.containers.internal:')[1].split('/')[0])
                        socket.create_connection(('127.0.0.1', port), timeout=2).close()
                        reachable.append(port)
                    return await plain_exec(**kwargs)

                environment.exec = exec_and_probe
                with patch.object(adapter, 'read_key_file', return_value=secret):
                    await agent.run('fixture instruction', environment, None)
                content = environment.uploads[0]['content']
                self.assertNotIn(secret, content)
                self.assertNotIn(secret[:8], content)
                self.assertNotIn('api.example.test', content, 'container must only know the proxy')
                (port,) = reachable
                container_provider = f'custom:http://host.containers.internal:{port}/v1'
                self.assertIn(json.dumps(container_provider), content)
                with self.assertRaises(OSError, msg='proxy must be stopped after the trial'):
                    socket.create_connection(('127.0.0.1', port), timeout=2).close()
                self.assertEqual(agent._metadata['provider'], 'api.example.test')
                self.assertEqual(agent._metadata['settlement_provider'], container_provider)
                expected = unavailable('config unreadable: TimeoutError') if mode == 'download_timeout' else 'clean'
                self.assertEqual(agent._metadata['container_config_key_scan'], expected)
                self.assertEqual(environment.calls[-1]['command'], 'rm -rf /opt/prx-bench')
                hashes.add(agent._metadata['profile_sha256'])
        self.assertEqual(len(hashes), 1, 'profile hash must not depend on the per-trial token or port')

    @deadline(60)
    async def test_key_found_in_container_config_after_trial_fails_the_trial(self):
        secret = 'prx-fixture-secret-not-exported'
        for leaked in (secret, secret[:8]):
            with self.subTest(leaked=leaked):
                agent = self.proxied_agent()
                environment = FakeEnvironment('normal')

                async def download(remote, local, leaked=leaked):
                    Path(local).write_text(f'api_key = "{leaked}"\n' if remote.endswith('config.toml') else 'x')

                environment.download_file = download
                with patch.object(adapter, 'read_key_file', return_value=secret):
                    with self.assertRaisesRegex(RuntimeError, 'credential leak in container config'):
                        await agent.run('fixture instruction', environment, None)
                self.assertEqual(agent._metadata['container_config_key_scan'], 'leak')
                self.assertEqual(environment.calls[-1]['command'], 'rm -rf /opt/prx-bench')

    @deadline(60)
    async def test_only_proxyable_providers_are_accepted(self):
        for refused in ('openai', 'anthropic', 'moonshot'):
            with self.subTest(provider=refused), self.assertRaisesRegex(ValueError, 'host key proxy'):
                adapter.require_proxy_route(refused)
        for accepted in ('kimi-code', 'kimi_coding', 'custom:https://api.example.test/v1'):
            adapter.require_proxy_route(accepted)

    @deadline(60)
    async def test_built_in_provider_keeps_its_name_and_gets_the_proxy_as_api_url(self):
        secret = 'prx-fixture-secret-not-exported'
        for mode in ('normal', 'timeout'):
            with self.subTest(mode=mode):
                agent = self.proxied_agent()
                agent.model_name = 'kimi-code/kimi-for-coding'
                agent.provider_override = None
                agent.provider = 'kimi-code'
                agent.model = 'kimi-for-coding'
                environment = FakeEnvironment(mode)
                with patch.object(adapter, 'read_key_file', return_value=secret):
                    await agent.run('fixture instruction', environment, None)
                content = environment.uploads[0]['content']
                port = int(content.split('host.containers.internal:')[1].split('/')[0])
                self.assertIn('default_provider = "kimi-code"\n', content)
                self.assertIn(f'api_url = "http://host.containers.internal:{port}/coding/v1"\n', content)
                self.assertNotIn('api.kimi.com', content, 'container must only know the proxy')
                self.assertNotIn('[[providers.models]]', content)
                self.assertNotIn(secret[:8], content)
                with self.assertRaises(OSError, msg='proxy must be stopped after the trial'):
                    socket.create_connection(('127.0.0.1', port), timeout=2).close()
                self.assertEqual(agent._metadata['provider'], 'kimi-code')
                self.assertEqual(agent._metadata['settlement_provider'], 'kimi-code')
                self.assertNotIn('api_url', agent._metadata['profile_values'])
                invocation = next(c['command'] for c in environment.calls if 'agent -t' in c['command'])
                self.assertIn('NO_PROXY=host.containers.internal', invocation)
                self.assertEqual(environment.calls[-1]['command'], 'rm -rf /opt/prx-bench')

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
    def test_deadline_cleanup_kills_separate_session_descendants(self):
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
                # Model Harbor's host-side wait deadline without signaling PRX.
                with self.assertRaises(subprocess.TimeoutExpired):
                    parent.wait(timeout=0.2)
                self.assertIsNone(parent.poll(), 'PRX must remain alive at the exec deadline')
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
