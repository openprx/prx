"""Harbor 0.25.0 installed-agent adapter; all accounting lives in record.py.

The provider key never enters the task container. The adapter runs
key_proxy.KeyProxy on the host for one trial and the container profile holds
the proxy URL plus a per-trial token: a `custom:https://` provider is renamed
to the proxy URL, and a built-in provider in BUILTIN_UPSTREAMS keeps its name
(and so its catalog entry) and gets the proxy URL as the root `api_url`. A
plain `custom:http://` provider is the unpaid smoke endpoint: it is reached
directly and also receives a throwaway token, never the key file content.
"""
import asyncio
import logging
import math
import os
import secrets
import sqlite3
import stat
from pathlib import Path
import shlex
import tempfile

from harbor.agents.installed.base import BaseInstalledAgent

from key_proxy import CONTAINER_HOST, holds_key, open_proxy, primary_ipv4
from profile import PRICES, check, identity, read_bounded, render
from record import AGENT_KEYS, agent_metadata, build_agent, endpoint, sha256, unavailable, per_attempt_models

CONFIG_DIR = "/opt/prx-bench"
TASK_WORKDIR = "/app"
LOG_DIR = "/logs/agent"
# Freeze parents before discovery so children cannot escape by reparenting.
# The caller also enforces a ten-second exec deadline.
KILL_PRX = r"""
exe=${PRX_EXE:-/usr/local/bin/prx}
pids=" "
for path in /proc/[0-9]*; do
    if [ "$(readlink "$path/exe" 2>/dev/null)" = "$exe" ]; then
        pid=${path#/proc/}
        kill -STOP "$pid" 2>/dev/null || true
        pids="$pids$pid "
    fi
done
roots=$pids
pass=0
while [ "$pass" -lt 32 ]; do
    added=0
    tree=$(awk '{sub(/.*\) /, ""); print FILENAME, $2}' /proc/[0-9]*/stat 2>/dev/null)
    while read -r path parent; do
        pid=${path#/proc/}; pid=${pid%/stat}
        case "$pids" in *" $pid "*) continue ;; esac
        case "$pids" in
            *" $parent "*)
                kill -STOP "$pid" 2>/dev/null || true
                pids="$pids$pid "
                added=1 ;;
        esac
    done <<EOF
$tree
EOF
    [ "$added" = 1 ] || break
    pass=$((pass + 1))
done
for pid in $pids; do
    case "$roots" in *" $pid "*) continue ;; esac
    # A negative pid kills a group only when this pid leads that group.
    kill -KILL -- "-$pid" 2>/dev/null || true
    kill -KILL "$pid" 2>/dev/null || true
done
for pid in $roots; do
    kill -KILL -- "-$pid" 2>/dev/null || true
    kill -KILL "$pid" 2>/dev/null || true
done
true
"""


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


# Built-in providers whose endpoint PRX lets the root `api_url` replace, with
# the fixed upstream the proxy forwards to. Keep in step with the factory arm
# in src/providers/mod.rs.
KIMI_CODE = "https://api.kimi.com/coding/v1"
BUILTIN_UPSTREAMS = {"kimi-code": KIMI_CODE, "kimi_coding": KIMI_CODE, "kimi_for_coding": KIMI_CODE}


def require_proxy_route(provider):
    # Any other built-in provider has a fixed base URL in PRX, so the key
    # would have to enter the container. Fail closed instead.
    if not provider.startswith("custom:") and provider not in BUILTIN_UPSTREAMS:
        raise ValueError("this built-in provider cannot use the host key proxy; "
                         "pass provider=custom:https://<provider base URL>")


class PrxAgent(BaseInstalledAgent):
    def __init__(self, *args, binary_path, binary_sha256, git_sha,
                 agent_timeout_sec, key_file, temperature=0.0, provider=None, proxy_bind=None, proxy_peers=None, **kwargs):
        self.key_file = Path(key_file)
        self.binary_path = Path(binary_path)
        self.binary_sha256 = str(binary_sha256)
        self.git_sha = str(git_sha)
        self.agent_timeout_sec = float(agent_timeout_sec)
        self.temperature = float(temperature)
        if not math.isfinite(self.agent_timeout_sec) or self.agent_timeout_sec <= 50:
            raise ValueError("agent_timeout_sec must exceed the 50 second cleanup reserve")
        if not math.isfinite(self.temperature):
            raise ValueError("temperature must be finite")
        self.prices = {key: float(kwargs.pop("price_" + key)) for key in PRICES}
        self.provider_override = provider
        self.proxy_bind = proxy_bind
        # Optional comma-separated source networks; unset leaves the token as the only gate.
        self.proxy_peers = proxy_peers
        super().__init__(*args, **kwargs)
        self.provider, self.model = identity(self.model_name or "", provider)
        require_proxy_route(self.provider)
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
        require_proxy_route(self.provider)
        proxy = None
        api_url = None
        container_provider = self.provider
        if self.provider in BUILTIN_UPSTREAMS:
            proxy = await self._proxy(BUILTIN_UPSTREAMS[self.provider], key)
            api_url = proxy.container_base_url()
        elif self.provider.startswith("custom:https://"):
            proxy = await self._proxy(self.provider[7:], key)
            container_provider = "custom:" + proxy.container_base_url()
        try:
            if proxy is not None:
                proxy.start()
            await self._trial(instruction, environment, key, proxy, container_provider, api_url)
        finally:
            # Synchronous on purpose: cancellation cannot skip it. A handler
            # that outlives stop() could still spend, so the trial fails.
            if proxy is not None and not proxy.stop():
                raise RuntimeError("key proxy did not stop cleanly")

    async def _proxy(self, upstream, key):
        # DNS runs off the event loop under a startup deadline; failure here
        # fails the trial before anything is started.
        peers = [peer for peer in str(self.proxy_peers).split(",") if peer.strip()] if self.proxy_peers else None
        return await open_proxy(upstream, key, self.proxy_bind or primary_ipv4(), allowed_peers=peers)

    async def _trial(self, instruction, environment, key, proxy, container_provider, api_url):
        credential = proxy.token if proxy is not None else secrets.token_urlsafe(32)
        with tempfile.TemporaryDirectory(prefix="prx-bench-profile-") as directory:
            path = Path(directory) / "config.toml"
            override = container_provider if container_provider.startswith("custom:") else None
            profile_text = render(path, self.model_name, credential, self.prices, override, api_url)
            values = check(path)
        # The per-trial proxy URL is covered by the canonical hash, not the record.
        values.pop("api_url", None)
        if holds_key(profile_text.encode(), key):
            raise RuntimeError("refusing to upload a profile that contains the provider key")
        # Hash a canonical profile: the token and proxy port change every trial.
        canonical = profile_text.replace(credential, "<per-trial-credential>")
        if proxy is not None:
            canonical = canonical.replace(f"{CONTAINER_HOST}:{proxy.address[1]}", f"{CONTAINER_HOST}:<per-trial-port>")
        self._metadata = agent_metadata(values, self.provider, self.model, self.prices,
            self.temperature, self.git_sha, self.binary_sha256, canonical,
            instruction, self.version(), self.agent_timeout_sec, settlement=container_provider)
        self._metadata["workdir"] = TASK_WORKDIR
        self._metadata["container_config_key_scan"] = unavailable("not run")
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
            seconds = self.agent_timeout_sec - 50
            host = shlex.quote(CONTAINER_HOST if proxy is not None else endpoint(container_provider))
            no_proxy = f"NO_PROXY={host} no_proxy={host} "
            command = (f"env -u PRX_BENCH_API_KEY {no_proxy}prx --config-dir {CONFIG_DIR} "
                       f"agent -t {self.temperature:g} --workdir {TASK_WORKDIR} -m {shlex.quote(instruction)} "
                       f">{LOG_DIR}/prx-stdout.txt 2>{LOG_DIR}/prx-stderr.txt; "
                       f"code=$?; printf '%s\\n' \"$code\" >{LOG_DIR}/prx-exit-code")
            try:
                await environment.exec(command=command, user="root", timeout_sec=seconds)
            except (TimeoutError, asyncio.TimeoutError):
                await environment.exec(command=KILL_PRX, user="root", timeout_sec=10)
                await environment.exec(command=f"printf '124\\n' >{LOG_DIR}/prx-exit-code", timeout_sec=5)
            except RuntimeError as error:
                # Harbor 0.25 DockerEnvironment translates its asyncio timeout.
                if not str(error).startswith("Command timed out after "):
                    raise
                await environment.exec(command=KILL_PRX, user="root", timeout_sec=10)
                await environment.exec(command=f"printf '124\\n' >{LOG_DIR}/prx-exit-code", timeout_sec=5)
        finally:
            proxy_stopped = proxy is None or proxy.stop()
            leaked = False
            try:
                await environment.exec(command=KILL_PRX, user="root", timeout_sec=10)
                try:
                    leaked = await asyncio.wait_for(self._config_holds_key(environment, key), timeout=15)
                    self._metadata["container_config_key_scan"] = "leak" if leaked else "clean"
                except (ValueError, OSError, asyncio.TimeoutError, RuntimeError, sqlite3.Error) as error:
                    self._metadata["container_config_key_scan"] = unavailable(f"config unreadable: {type(error).__name__}")
                    logging.getLogger("prx_bench.adapter").warning("container config key scan did not run")
                await asyncio.wait_for(self._collect(environment), timeout=15)
            except (sqlite3.Error, ValueError, KeyError, TypeError, OSError, asyncio.TimeoutError, RuntimeError) as error:
                self._metadata["per_attempt_models"] = unavailable(f"per-attempt ledger unreadable: {type(error).__name__}")
            finally:
                await asyncio.shield(environment.exec(command="rm -rf /opt/prx-bench", user="root", timeout_sec=5))
            if not proxy_stopped:
                raise RuntimeError("key proxy did not stop cleanly")
            if leaked:
                raise RuntimeError("credential leak in container config")

    async def _config_holds_key(self, environment, key):
        """Download the container-side config after the trial and scan it on the host."""
        with tempfile.TemporaryDirectory(prefix="prx-bench-scan-") as directory:
            local = Path(directory) / "config.toml"
            await environment.download_file(CONFIG_DIR + "/config.toml", str(local))
            return holds_key(read_bounded(local), key)

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
