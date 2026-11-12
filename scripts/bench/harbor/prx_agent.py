"""Harbor 0.25.0 installed-agent adapter; all accounting lives in record.py."""
import asyncio
import math
import os
import sqlite3
import stat
from pathlib import Path
import shlex
import tempfile

from harbor.agents.installed.base import BaseInstalledAgent

from profile import PRICES, check, identity, read_bounded, render
from record import AGENT_KEYS, agent_metadata, build_agent, endpoint, sha256, unavailable, per_attempt_models

CONFIG_DIR = "/opt/prx-bench"
LOG_DIR = "/logs/agent"
KILL_PRX = 'for p in /proc/[0-9]*; do [ "$(readlink $p/exe 2>/dev/null)" = /usr/local/bin/prx ] && kill -KILL ${p#/proc/}; done; true'


def read_key_file(path):
    path = Path(path)
    if path.parent != Path("/opt/ops/bench/secrets"):
        raise ValueError("key_file must be under /opt/ops/bench/secrets/")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600 or info.st_uid != os.getuid():
            raise ValueError("key_file must be owner-only (0600)")
        key = stream.read(65537)
    if len(key) > 65536 or not key.strip():
        raise ValueError("invalid key_file")
    return key.decode().strip()


class PrxAgent(BaseInstalledAgent):
    def __init__(self, *args, binary_path, binary_sha256, git_sha,
                 agent_timeout_sec, key_file, temperature=0.0, provider=None, **kwargs):
        self.key_file = Path(key_file)
        self.binary_path = Path(binary_path)
        self.binary_sha256 = str(binary_sha256)
        self.git_sha = str(git_sha)
        self.agent_timeout_sec = float(agent_timeout_sec)
        self.temperature = float(temperature)
        if not math.isfinite(self.agent_timeout_sec) or self.agent_timeout_sec <= 30:
            raise ValueError("agent_timeout_sec must exceed the 30 second cleanup reserve")
        if not math.isfinite(self.temperature):
            raise ValueError("temperature must be finite")
        self.prices = {key: float(kwargs.pop("price_" + key)) for key in PRICES}
        self.provider_override = provider
        super().__init__(*args, **kwargs)
        self.provider, self.model = identity(self.model_name or "", provider)
        self._metadata = {key: unavailable("agent did not produce metadata") for key in AGENT_KEYS}

    @staticmethod
    def name():
        return "prx"

    def get_version_command(self):
        return "prx --version"

    async def install(self, environment):
        if sha256(read_bounded(self.binary_path, 256 * 1024 * 1024)) != self.binary_sha256:
            raise ValueError("binary sha256 mismatch")
        await environment.upload_file(self.binary_path, "/usr/local/bin/prx")
        result = await environment.exec(command="chmod 0755 /usr/local/bin/prx", user="root", timeout_sec=30)
        if result.return_code != 0:
            raise RuntimeError("cannot install PRX binary")
        version = await environment.exec(command="prx --version", timeout_sec=30)
        if version.return_code != 0 or not version.stdout:
            raise RuntimeError("cannot read installed PRX version")
        self._version = version.stdout.strip()

    async def run(self, instruction, environment, context):
        # Keep context empty: Harbor invokes populate_context_post_run after log download.
        del context
        key = read_key_file(self.key_file)
        with tempfile.TemporaryDirectory(prefix="prx-bench-profile-") as directory:
            path = Path(directory) / "config.toml"
            profile_text = render(path, self.model_name, key, self.prices, self.provider_override)
            values = check(path)
        self._metadata = agent_metadata(values, self.provider, self.model, self.prices,
            self.temperature, self.git_sha, self.binary_sha256, profile_text,
            instruction, self.version(), self.agent_timeout_sec)
        setup = await environment.exec(
            command="test ! -e /opt/prx-bench && mkdir -p /opt/prx-bench /logs/agent && chown root:root /opt/prx-bench && chmod 0700 /opt/prx-bench",
            user="root", timeout_sec=30)
        if setup.return_code != 0:
            raise RuntimeError("cannot create private config directory")
        try:
            await self._upload_config_text(environment, content=profile_text,
                                           remote_path=CONFIG_DIR + "/config.toml", filename="config.toml")
            locked = await environment.exec(
                command="chown root:root /opt/prx-bench/config.toml && chmod 0600 /opt/prx-bench/config.toml",
                user="root", timeout_sec=30)
            if locked.return_code != 0:
                raise RuntimeError("cannot secure private config")
            timeout_available = await environment.exec(command="command -v timeout", timeout_sec=10)
            seconds = self.agent_timeout_sec - 30
            timeout_prefix = f"timeout -k 5s {seconds:g}s " if timeout_available.return_code == 0 else ""
            no_proxy = f"NO_PROXY={shlex.quote(endpoint(self.provider))} no_proxy={shlex.quote(endpoint(self.provider))} " if self.provider.startswith("custom:") else ""
            command = (f"env -u PRX_BENCH_API_KEY {no_proxy}{timeout_prefix}prx --config-dir {CONFIG_DIR} "
                       f"agent -t {self.temperature:g} -m {shlex.quote(instruction)} "
                       f">{LOG_DIR}/prx-stdout.txt 2>{LOG_DIR}/prx-stderr.txt; "
                       f"code=$?; printf '%s\\n' \"$code\" >{LOG_DIR}/prx-exit-code")
            try:
                await environment.exec(command=command, user="root", timeout_sec=seconds + (15 if timeout_prefix else 0))
            except (TimeoutError, asyncio.TimeoutError):
                await environment.exec(command=KILL_PRX, user="root", timeout_sec=5)
                await environment.exec(command=f"printf '124\\n' >{LOG_DIR}/prx-exit-code", timeout_sec=5)
            except RuntimeError as error:
                # Harbor 0.25 DockerEnvironment translates its asyncio timeout.
                if not str(error).startswith("Command timed out after "):
                    raise
                await environment.exec(command=KILL_PRX, user="root", timeout_sec=5)
                await environment.exec(command=f"printf '124\\n' >{LOG_DIR}/prx-exit-code", timeout_sec=5)
        finally:
            try:
                await environment.exec(command=KILL_PRX, user="root", timeout_sec=5)
                await asyncio.wait_for(self._collect(environment), timeout=15)
            except (sqlite3.Error, ValueError, KeyError, TypeError, OSError, asyncio.TimeoutError, RuntimeError) as error:
                self._metadata["per_attempt_models"] = unavailable(f"per-attempt ledger unreadable: {type(error).__name__}")
            finally:
                await asyncio.shield(environment.exec(command="rm -rf /opt/prx-bench", user="root", timeout_sec=5))

    async def _collect(self, environment):
        await environment.exec(
            command=(f"test ! -f {CONFIG_DIR}/workspace/state/costs.jsonl || "
                     f"cp {CONFIG_DIR}/workspace/state/costs.jsonl {LOG_DIR}/costs.jsonl; "
                     f"env -u PRX_BENCH_API_KEY prx --config-dir {CONFIG_DIR} cost --json "
                     f">{LOG_DIR}/prx-cost.json 2>/dev/null || rm -f {LOG_DIR}/prx-cost.json"),
            user="root", timeout_sec=15)
        with tempfile.TemporaryDirectory(prefix="prx-bench-events-") as directory:
            database = Path(directory) / "brain.db"
            for suffix in ("", "-wal"):
                remote = CONFIG_DIR + "/workspace/memory/brain.db" + suffix
                exists = await environment.exec(command=f"test -f {remote} && test $(wc -c < {remote}) -le 67108864", user="root", timeout_sec=5)
                if exists.return_code == 0:
                    await asyncio.wait_for(environment.download_file(remote, str(database) + suffix), timeout=10)
            self._metadata["per_attempt_models"] = per_attempt_models(database, self.git_sha)

    def populate_context_post_run(self, context):
        agent = build_agent(self.logs_dir, self._metadata)
        totals = agent["usage"]
        if totals.get("status") != "unavailable":
            context.n_input_tokens = totals["uncached_input"] + totals["cache_read"] + totals["cache_write"]
            context.n_cache_tokens = totals["cache_read"]
            context.n_output_tokens = totals["output"]
            context.cost_usd = totals["cost_usd"]
        context.metadata = {**(context.metadata or {}), "prx_trial_agent": agent}
