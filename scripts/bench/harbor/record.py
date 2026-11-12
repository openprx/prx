"""Two-stage trial records; this module has no Harbor dependency."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
import tomllib
from urllib.parse import urlsplit, urlunsplit
from profile import read_bounded

MAX_ARTIFACT = 16 * 1024 * 1024
TRANSPORT = re.compile(r"HTTP[^\n]*(?:5\d\d|429)|status[^\n]*(?:5\d\d|429)|connect|TLS|DNS|certificate|name resolution", re.I)


def unavailable(reason):
    return {"status": "unavailable", "reason": reason}


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def load(path):
    return json.loads(read_bounded(path, MAX_ARTIFACT))


def optional(path):
    return load(path) if path.is_file() else unavailable(f"missing {path.name}")


def text(path):
    return read_bounded(path, MAX_ARTIFACT).decode(errors="replace") if path.is_file() else ""


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def endpoint(provider):
    if provider.startswith("custom:"):
        return urlsplit(provider[7:]).hostname or "unknown"
    return provider


def serving_provider(provider):
    if not provider.startswith("custom:"):
        return provider
    url = urlsplit(provider[7:])
    host = url.hostname or ""
    if ":" in host:
        host = f"[{host}]"
    if url.port:
        host += f":{url.port}"
    return "custom:" + urlunsplit((url.scheme, host, url.path, url.query, url.fragment))


def usage(rows, exit_code):
    if not rows:
        return unavailable("killed_before_settlement" if exit_code == 124 else "no_settled_usage")
    result = {"uncached_input": 0, "cache_read": 0, "cache_write": 0, "output": 0,
              "reasoning": unavailable("not exported by PRX"), "cost_usd": 0.0}
    for row in rows:
        u = row["usage"]
        cache = row.get("cache_read_input_tokens")
        creation = row.get("cache_creation_input_tokens")
        if cache is None or creation is None:
            return unavailable("ledger cache accounting unavailable")
        if cache > u["input_tokens"]:
            raise ValueError("cache tokens exceed input")
        result["uncached_input"] += u["input_tokens"] - cache
        result["cache_read"] += cache
        result["cache_write"] += creation
        result["output"] += u["output_tokens"]
        result["cost_usd"] += u["cost_usd"]
    return result


def build_agent(logs, metadata):
    logs = Path(logs)
    code_path = logs / "prx-exit-code"
    exit_code = int(text(code_path).strip()) if code_path.is_file() else unavailable("exit code not collected")
    ledger = logs / "costs.jsonl"
    rows = [json.loads(line) for line in text(ledger).splitlines() if line.strip()]
    result = {**metadata, "exit_code": exit_code, "costs_rows": rows,
              "stdout_sha256": sha256(read_bounded(logs / "prx-stdout.txt", MAX_ARTIFACT)) if (logs / "prx-stdout.txt").is_file() else unavailable("stdout not collected"),
              "stderr_sha256": sha256(read_bounded(logs / "prx-stderr.txt", MAX_ARTIFACT)) if (logs / "prx-stderr.txt").is_file() else unavailable("stderr not collected"),
              "cost_report": optional(logs / "prx-cost.json")}
    result["usage"] = usage(rows, exit_code)
    write(logs / "prx-agent-side.json", result)
    return result


def classify(agent, result, reward, stdout, stderr):
    exception = result.get("exception_info") or {}
    exception_name = exception.get("exception_type", "")
    code = agent["exit_code"]
    provider, model = agent["settlement_provider"], agent["model_id"]
    if any(row.get("usage", {}).get("model") != f"{provider}/{model}" or
           row.get("settlement_provider") != provider for row in agent["costs_rows"]):
        return "infra_error", "model_substitution"
    if exception_name == "AgentTimeoutError" or code == 124:
        return "timeout", "agent_timeout"
    if exception:
        return "infra_error", "harbor_exception"
    if code == 2:
        if re.search(r"^Reason:.*(?:price|budget)", stdout, re.I | re.M):
            return "infra_error", "config_refusal"
        return "refusal", "harness_policy"
    if code == 1:
        return ("infra_error", "provider_transport") if TRANSPORT.search(stderr) else ("fail", "agent_error")
    if code == 0:
        return ("pass", "verifier_reward") if reward == 1.0 else ("fail", "verifier_reward")
    return "infra_error", "missing_or_unknown_exit"


def finalize(trial):
    trial = Path(trial)
    result, config = load(trial / "result.json"), load(trial / "config.json")
    logs = trial / "agent"
    agent_path = logs / "prx-agent-side.json"
    if agent_path.is_file():
        agent = load(agent_path)
    else:
        # Setup/Harbor failures can precede run() and log download.
        absent = unavailable("agent did not produce metadata")
        agent = build_agent(logs, {key: absent for key in (
            "git_sha", "binary_sha256", "profile_sha256", "instruction_sha256", "version",
            "model_id", "provider", "settlement_provider", "sampling", "profile_values",
            "agent_timeout_sec", "per_attempt_models")})
    task_path = Path(config["task"]["path"])
    # Host launcher passes absolute local task paths; git task caches are not guessed.
    task_file = task_path / "task.toml"
    task = tomllib.loads(read_bounded(task_file).decode())
    env = task.get("environment", {})
    rewards = (result.get("verifier_result") or {}).get("rewards") or {}
    reward = rewards.get("reward", unavailable("verifier reward missing"))
    status, reason = classify(agent, result, reward, text(logs / "prx-stdout.txt"), text(logs / "prx-stderr.txt"))
    artifacts = {}
    for name in ("agent/prx-stdout.txt", "agent/prx-stderr.txt", "agent/costs.jsonl", "agent/prx-cost.json",
                 "verifier/reward.txt", "verifier/ctrf.json", "verifier/test-stdout.txt", "trial.log"):
        path = trial / name
        artifacts[name] = sha256(read_bounded(path, MAX_ARTIFACT)) if path.is_file() else unavailable("not produced")
    phases = {key: result.get(key) or unavailable(f"missing {key}") for key in
              ("environment_setup", "agent_setup", "agent_execution", "verifier")}
    record = {
        "schema_version": 1, "run_id": config.get("job_id") or unavailable("job ID missing"),
        "trial_id": result["id"], "created_at": result["started_at"],
        "benchmark": "terminal-bench", "dataset_tag": "2.0", "task_id": result["task_name"],
        "repo_revision": config["task"].get("git_commit_id") or unavailable("local task has no git revision"),
        "task_checksum": result.get("task_checksum") or unavailable("task checksum missing"),
        "image": env.get("docker_image") or unavailable("no image declared"),
        "image_digest": unavailable("image not inspected"),
        "harness_name": "prx", "version": agent["version"] or unavailable("version not detected"), "git_sha": agent["git_sha"],
        "binary_sha256": agent["binary_sha256"], "profile_sha256": agent["profile_sha256"],
        "instruction_sha256": agent["instruction_sha256"],
        "harness_prompt_sha256": unavailable("runtime-built prompt not exported; covered by binary and profile"),
        "model_id": agent["model_id"], "provider": agent["provider"],
        "sampling": agent["sampling"], "profile_values": agent["profile_values"],
        "resource_limits": {"cpus": env.get("cpus", unavailable("not declared")),
                            "memory_mb": env.get("memory_mb", env.get("memory", unavailable("not declared"))),
                            "storage_mb": env.get("storage_mb", env.get("storage", unavailable("not declared"))),
                            "gpu": unavailable("v1 GPU inspection unavailable"),
                            "agent_timeout_sec": agent["agent_timeout_sec"],
                            "network_mode": task.get("agent", {}).get("network", {}).get("network_mode", "public")},
        "score": reward, "verifier_details": {"result": result.get("verifier_result") or unavailable("no verifier result"),
            "reward_txt": text(trial / "verifier/reward.txt") or unavailable("not produced"),
            "ctrf": optional(trial / "verifier/ctrf.json"),
            "stdout": text(trial / "verifier/test-stdout.txt") or unavailable("not produced")},
        "attempt_status": status, "reason": reason, "classifier": "v1-exitcode",
        "usage": agent["usage"], "cost": agent["usage"].get("cost_usd", agent["usage"]),
        "unsettled_dispatched_requests": agent["cost_report"].get("unsettled_dispatched_requests", unavailable("cost report missing")),
        "per_attempt_models": agent["per_attempt_models"], "timings": {"queue": unavailable("not exported"), **phases},
        "trajectory": {"stdout": "agent/prx-stdout.txt", "log": "trial.log", "commands": unavailable("not exported")},
        "diff": unavailable("not exported"), "artifacts": artifacts,
        "harness_limits": ["shell_git_refused", "cwd_not_task_workdir"],
    }
    if status == "refusal":
        record["refusal_kind"] = "harness_policy"
    write(logs / "prx-trial.json", record)
    return record


def scan_secrets(trial, secret):
    if not secret:
        raise ValueError("secret required for scan")
    names = ("agent/prx-trial.json", "agent/prx-agent-side.json", "agent/prx-stdout.txt",
             "agent/prx-stderr.txt", "config.json", "result.json", "trial.log")
    for name in names:
        path = Path(trial) / name
        if path.is_file():
            data = read_bounded(path, MAX_ARTIFACT)
            if any(token.encode() in data for token in (secret, secret[:8])):
                raise ValueError(f"credential leak in {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["finalize"])
    parser.add_argument("trial_dir", type=Path)
    args = parser.parse_args()
    try:
        finalize(args.trial_dir)
    except (ValueError, OSError, KeyError, TypeError):
        print("cannot finalize trial: invalid or missing artifact", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
