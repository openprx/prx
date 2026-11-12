"""Opt-in QA proof of Harbor exec deadlines and PRX descendant cleanup.

Run with Harbor 0.25.0 and its Docker/Podman environment configured.
--break-timeout restores the removed GNU wrapper and must fail the marker check.
"""
import argparse
import asyncio
from pathlib import Path
import shlex
import re
import tempfile
import tomllib
import uuid

from prx_agent import KILL_PRX


async def prove(task, output, break_timeout):
    from harbor.environments.docker.docker import DockerEnvironment
    from harbor.models.task.config import TaskConfig
    from harbor.models.trial.paths import TrialPaths

    config = TaskConfig.model_validate(tomllib.loads((task / 'task.toml').read_text()))
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='deadline-', dir=output) as directory:
        paths = TrialPaths(Path(directory))
        paths.mkdir()
        paths.chmod_dir()
        name = 'prx-deadline-' + uuid.uuid4().hex[:12]
        environment = DockerEnvironment(environment_dir=task / 'environment',
            environment_name=name, session_id=name, trial_paths=paths,
            task_env_config=config.environment)
        try:
            await asyncio.wait_for(environment.start(force_build=False), timeout=120)
            setup = await environment.exec(command='cp /bin/dash /usr/local/bin/prx',
                                           user='root', timeout_sec=10)
            assert setup.return_code == 0, 'cannot install stand-in executable'
            child = 'echo $$ > /logs/agent/child.pid; while :; do echo x >> /logs/agent/marker; sleep 0.1; done'
            parent = 'echo $$ > /logs/agent/root.pid; setsid sh -c ' + shlex.quote(child) + ' & wait'
            prefix = 'timeout -k 5s 2s ' if break_timeout else ''
            command = prefix + '/usr/local/bin/prx -c ' + shlex.quote(parent)
            deadline_raised = False
            try:
                result = await environment.exec(command=command, user='root', timeout_sec=10)
                print(f'exec returned: {result.return_code}', flush=True)
            except RuntimeError as error:
                if not str(error).startswith('Command timed out after '):
                    raise
                deadline_raised = True
                print(f'Harbor deadline: {error}', flush=True)
            alive = await environment.exec(
                command='root=$(cat /logs/agent/root.pid); '
                    'test "$(readlink /proc/$root/exe)" = /usr/local/bin/prx',
                user='root', timeout_sec=10)
            print(f'PRX root alive after exec: {alive.return_code == 0}', flush=True)
            if not break_timeout:
                assert deadline_raised, 'Harbor did not enforce the exec deadline'
                assert alive.return_code == 0, 'BLOCKED: Harbor killed the PRX root'
            await environment.exec(command=KILL_PRX, user='root', timeout_sec=10)
            observation = await environment.exec(command=
                'before=$(wc -c < /logs/agent/marker); sleep 1; '
                'after=$(wc -c < /logs/agent/marker); printf "PRX_MARKER:%s:%s\\n" "$before" "$after"',
                user='root', timeout_sec=10)
            assert observation.return_code == 0, 'cannot observe marker'
            match = re.search(r'PRX_MARKER:(\d+):(\d+)', observation.stdout)
            assert match is not None, 'marker observation missing from container output'
            size, after = map(int, match.groups())
            assert size >= 4, 'child marker must grow before cleanup'
            print(f'marker bytes after cleanup / one second later: {size}/{after}', flush=True)
            assert after == size, 'detached child survived deadline cleanup'
            print('Harbor deadline descendant cleanup: PASS', flush=True)
        finally:
            # Kill only this proof container's known child group, including on BtF failure.
            try:
                await environment.exec(command='if test -f /logs/agent/child.pid; then '
                    'kill -KILL -- -$(cat /logs/agent/child.pid) 2>/dev/null || true; fi',
                    user='root', timeout_sec=10)
            finally:
                await asyncio.wait_for(environment.stop(delete=True), timeout=60)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--task', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--break-timeout', action='store_true')
    args = parser.parse_args()
    asyncio.run(asyncio.wait_for(prove(args.task, args.output, args.break_timeout), timeout=240))


if __name__ == '__main__':
    main()
