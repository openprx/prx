"""Two-stage trial records; this module has no Harbor dependency."""
import argparse
from contextlib import closing
import hashlib
import json
from pathlib import Path
import re
import sys
import sqlite3
import time
import tomllib
from urllib.parse import urlsplit
from profile import read_bounded

MAX_ARTIFACT = 16 * 1024 * 1024
AGENT_KEYS = ("git_sha", "binary_sha256", "profile_sha256", "instruction_sha256", "version",
              "model_id", "provider", "settlement_provider", "sampling", "profile_values",
              "agent_timeout_sec", "per_attempt_models")
TRANSPORT = re.compile(r"API error \((?:5\d\d|429)\b|\b(?:HTTP|status)[ /:=]*(?:5\d\d|429)\b|error sending request|error trying to connect|tcp connect error|connection (?:refused|reset|closed)|dns error|failed to lookup address|name resolution|invalid peer certificate|tls handshake|request timed out|operation timed out", re.I)


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
    if not provider.startswith("custom:") or "@" not in provider:
        return provider
    return re.sub(r"(://)[^/?#]*@", r"\1[redacted]@", provider, count=1)


def agent_metadata(values, provider, model, prices, temperature, git_sha,
                   binary_sha256, profile_text, instruction, version, agent_timeout_sec):
    safe_values = {k: v for k, v in values.items()
                   if k not in ("api_key", "default_provider", "providers", "cost")}
    safe_values["default_provider"] = endpoint(provider)
    if provider.startswith("custom:"):
        safe_values["providers"] = {"models": [{"provider": endpoint(provider),
            "model": model, "operations": ["chat"], "native_tools": True}]}
    safe_values["cost"] = {**{k: v for k, v in values["cost"].items() if k != "prices"},
                           "prices": prices}
    return {
        "git_sha": git_sha, "binary_sha256": binary_sha256,
        "profile_sha256": sha256(profile_text.encode()),
        "instruction_sha256": sha256(instruction.encode()), "version": version,
        "model_id": model, "provider": endpoint(provider),
        "settlement_provider": serving_provider(provider),
        "sampling": {"temperature": temperature}, "profile_values": safe_values,
        "agent_timeout_sec": agent_timeout_sec,
        "per_attempt_models": unavailable(f"no per-attempt ledger at {git_sha}"),
    }


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
        if cache + creation > u["input_tokens"]:
            raise ValueError("cache tokens exceed input")
        result["uncached_input"] += u["input_tokens"] - cache - creation
        result["cache_read"] += cache
        result["cache_write"] += creation
        result["output"] += u["output_tokens"]
        result["cost_usd"] += u["cost_usd"]
    return result


def attempt_evidence(attempts):
    # FU-ATTEMPT-RECORD-RETRIES: persisted rows omit retries, and cost exports no retry counter.
    return {"status": "available", "attempts": attempts,
            "persisted_models": sorted({attempt["model"] for attempt in attempts}),
            "persisted_attempt_count": len(attempts),
            "attempt_count": unavailable("FU-ATTEMPT-RECORD-RETRIES: retry count not exported")}


def per_attempt_models(database, git_sha):
    """Read finalized provider attempts from a stopped trial's private SQLite copy."""
    database = Path(database)
    if not database.is_file():
        return unavailable(f"no per-attempt ledger at {git_sha}")
    if database.stat().st_size > 64 * 1024 * 1024:
        raise ValueError("event database exceeds benchmark collection bound")
    deadline = time.monotonic() + 5
    with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=1)) as connection:
        connection.set_progress_handler(lambda: int(time.monotonic() >= deadline), 1000)
        rows = connection.execute(
            "SELECT raw_payload_json FROM message_events WHERE event_type='turn.finalized' ORDER BY rowid LIMIT 257"
        ).fetchall()
    if len(rows) > 256:
        raise ValueError("too many finalized turns")
    attempts = []
    for (payload,) in rows:
        if payload is None:
            continue
        if len(payload) > MAX_ARTIFACT:
            raise ValueError("terminal payload exceeds collection bound")
        outcome = json.loads(payload).get("provider_outcome") or {}
        for attempt in outcome.get("attempts", []):
            if len(attempts) >= 256:
                raise ValueError("too many provider attempts")
            attempts.append({"seq": attempt["seq"], "provider": serving_provider(attempt["provider"]),
                             "model": attempt["model"], "status": attempt["status"]})
    return attempt_evidence(attempts) if attempts else unavailable(f"no per-attempt ledger at {git_sha}")


def build_agent(logs, metadata):
    logs = Path(logs)
    code_path = logs / "prx-exit-code"
    exit_code = int(raw) if (raw := text(code_path).strip()).lstrip("-").isdigit() else unavailable("exit code not collected")
    ledger = logs / "costs.jsonl"
    rows = []
    try:
        for line in text(ledger).splitlines():
            if line.strip():
                rows.append(json.loads(line))
        totals = usage(rows, exit_code)
    except (ValueError, KeyError, TypeError):
        rows = unavailable("ledger unreadable")
        totals = unavailable("ledger unreadable")
    result = {**metadata, "exit_code": exit_code, "costs_rows": rows,
              "stdout_sha256": sha256(read_bounded(logs / "prx-stdout.txt", MAX_ARTIFACT)) if (logs / "prx-stdout.txt").is_file() else unavailable("stdout not collected"),
              "stderr_sha256": sha256(read_bounded(logs / "prx-stderr.txt", MAX_ARTIFACT)) if (logs / "prx-stderr.txt").is_file() else unavailable("stderr not collected"),
              "cost_report": optional(logs / "prx-cost.json")}
    result["usage"] = totals
    write(logs / "prx-agent-side.json", result)
    return result


def classify(agent, result, reward, stdout, stderr):
    exception = result.get("exception_info") or {}
    exception_name = exception.get("exception_type", "")
    code = agent["exit_code"]
    provider, model = agent["settlement_provider"], agent["model_id"]
    if code == 124 and not agent["costs_rows"]:
        return "timeout", "agent_timeout"
    if not isinstance(provider, str) or not isinstance(model, str):
        return "infra_error", "missing_agent_metadata"
    if isinstance(agent["costs_rows"], dict):
        return "infra_error", "ledger_unreadable"
    if any(row.get("usage", {}).get("model") != f"{provider}/{model}" or
           row.get("settlement_provider") != provider for row in agent["costs_rows"]):
        return "infra_error", "model_substitution"
    evidence = agent.get("per_attempt_models", {})
    if evidence.get("status") == "available" and any(
            attempt.get("model") != model or attempt.get("provider") != provider
            for attempt in evidence.get("attempts", [])):
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


def resource_mb(environment, name):
    value = environment.get(name + "_mb")
    if value is not None:
        return value
    legacy = environment.get(name)
    if legacy is None:
        return unavailable("not declared")
    units = {"G": 1024, "M": 1, "K": 1 / 1024}
    suffix = str(legacy).strip().upper()
    if suffix[-1:] not in units:
        raise ValueError("invalid legacy resource unit")
    return int(float(suffix[:-1]) * units[suffix[-1:]])


def mark_unavailable(value):
    if value is None:
        return unavailable("not exported")
    if isinstance(value, dict):
        return {key: mark_unavailable(item) for key, item in value.items()}
    if isinstance(value, list):
        return [mark_unavailable(item) for item in value]
    return value


def finalize(trial):
    trial = Path(trial)
    result, config = load(trial / "result.json"), load(trial / "config.json")
    logs = trial / "agent"
    agent_path = logs / "prx-agent-side.json"
    absent = unavailable("agent did not produce metadata")
    if agent_path.is_file():
        agent = {**{k: absent for k in (*AGENT_KEYS, "usage", "cost_report")}, **load(agent_path)}
    else:
        # Setup/Harbor failures can precede run() and log download.
        agent = build_agent(logs, {key: absent for key in AGENT_KEYS})
    if agent["per_attempt_models"].get("status") == "available":
        agent["per_attempt_models"] = attempt_evidence(agent["per_attempt_models"]["attempts"])
    task_path = Path(config["task"]["path"])
    # Host launcher passes absolute local task paths; git task caches are not guessed.
    task_file = task_path / "task.toml"
    task = tomllib.loads(read_bounded(task_file).decode())
    env = task.get("environment", {})
    rewards = (result.get("verifier_result") or {}).get("rewards") or {}
    reward = rewards.get("reward", unavailable("verifier reward missing"))
    status, reason = classify(agent, result, reward, text(logs / "prx-stdout.txt"), text(logs / "prx-stderr.txt"))
    if status == "timeout" and agent["usage"].get("status") == "unavailable":
        agent["usage"] = unavailable("killed_before_settlement")
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
        "benchmark": "terminal-bench", "dataset_tag": "terminal-bench@2.0", "task_id": task_path.name,
        "repo_revision": config["task"].get("git_commit_id") or unavailable("local task has no git revision"),
        "task_checksum": result.get("task_checksum") or unavailable("task checksum missing"),
        "dataset_provenance": optional(task_path.parent / "prx-dataset.json"),
        "image": env.get("docker_image") or unavailable("no image declared"),
        "image_digest": unavailable("image not inspected"),
        "harness_name": "prx", "version": agent["version"] or unavailable("version not detected"), "git_sha": agent["git_sha"],
        "binary_sha256": agent["binary_sha256"], "profile_sha256": agent["profile_sha256"],
        "instruction_sha256": agent["instruction_sha256"],
        "harness_prompt_sha256": unavailable("runtime-built prompt not exported; covered by binary and profile"),
        "model_id": agent["model_id"], "provider": agent["provider"],
        "sampling": agent["sampling"], "profile_values": agent["profile_values"],
        "resource_limits": {"cpus": env.get("cpus", unavailable("not declared")),
                            "memory_mb": resource_mb(env, "memory"),
                            "storage_mb": resource_mb(env, "storage"),
                            "gpu": unavailable("v1 GPU inspection unavailable"),
                            "agent_timeout_sec": agent["agent_timeout_sec"],
                            "network_mode": task.get("agent", {}).get("network_mode") or env.get("network_mode", "public")},
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
    record = mark_unavailable(record)
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
