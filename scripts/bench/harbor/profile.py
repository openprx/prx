"""Render and validate the exact uploaded fixed-model benchmark profile."""
import argparse
import json
import math
import os
from pathlib import Path
import sys
import tomllib

MAX_CONFIG = 1024 * 1024
PRICES = ("input", "output", "cache_read", "cache_write")


def read_bounded(path, limit=MAX_CONFIG):
    with Path(path).open("rb") as stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"file exceeds {limit} bytes: {Path(path).name}")
    return data


def identity(model_name, provider_override=None):
    provider, sep, model = model_name.partition("/")
    if not sep or not provider or not model:
        raise ValueError("model must be provider/model")
    if provider.startswith("custom:"):
        raise ValueError("custom providers require an explicit provider adapter argument")
    if provider_override is not None:
        if not provider_override.startswith(("custom:http://", "custom:https://")):
            raise ValueError("provider override is only for custom HTTP smoke endpoints")
        provider = provider_override
    return provider, model


def check(path):
    config = tomllib.loads(read_bounded(path).decode())
    for table in ("router", "query_classification", "task_routing"):
        if "enabled" in config.get(table, {}):
            raise ValueError(f"removed gate: {table}.enabled")
    for table, key in (("reliability", "fallback_providers"),
                       ("reliability", "model_fallbacks"), ("reliability", "api_keys"),
                       ("query_classification", "rules"), ("task_routing", "rules"),
                       ("router", "models")):
        if config.get(table, {}).get(key):
            raise ValueError(f"fixed-model violation: {table}.{key}")
    for key in ("model_routes", "embedding_routes"):
        if config.get(key):
            raise ValueError(f"fixed-model violation: {key}")
    expected = tomllib.loads(read_bounded(Path(__file__).with_name("prx_bench_profile.toml")).decode())
    def verify(actual, required, prefix=""):
        for key, value in required.items():
            found = actual.get(key)
            if isinstance(value, dict):
                if not isinstance(found, dict):
                    raise ValueError(f"missing profile table: {prefix}{key}")
                verify(found, value, f"{prefix}{key}.")
            elif found != value:
                raise ValueError(f"incorrect profile value: {prefix}{key}")
    verify(config, expected)
    provider, model = config.get("default_provider"), config.get("default_model")
    if not provider or not model or not config.get("api_key"):
        raise ValueError("missing provider, model or credential")
    prices = config["cost"].get("prices", {}).get(f"{provider}/{model}", {})
    if set(prices) != set(PRICES) or any(
            isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) or v < 0
            for v in prices.values()):
        raise ValueError("all four finite nonnegative prices are required")
    declarations = config.get("providers", {}).get("models", [])
    wanted = [{"provider": provider, "model": model, "operations": ["chat"], "native_tools": True}]
    if declarations != (wanted if provider.startswith("custom:") else []):
        raise ValueError("incorrect model declaration")
    return config


def render(out, model_name, api_key, prices, provider_override=None):
    provider, model = identity(model_name, provider_override)
    if set(prices) != set(PRICES):
        raise ValueError("all four prices are required")
    q = json.dumps
    text = f"default_provider = {q(provider)}\ndefault_model = {q(model)}\napi_key = {q(api_key)}\n"
    text += read_bounded(Path(__file__).with_name("prx_bench_profile.toml")).decode()
    text += f"\n[cost.prices.{q(provider + '/' + model)}]\n"
    text += "".join(f"{key} = {float(prices[key])}\n" for key in PRICES)
    if provider.startswith("custom:"):
        text += f"\n[[providers.models]]\nprovider = {q(provider)}\nmodel = {q(model)}\noperations = [\"chat\"]\nnative_tools = true\n"
    out = Path(out)
    fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as stream:
        stream.write(text)
    os.chmod(out, 0o600)
    check(out)
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    validator = sub.add_parser("check")
    validator.add_argument("path")
    renderer = sub.add_parser("render")
    renderer.add_argument("--out", required=True)
    renderer.add_argument("--model", required=True)
    renderer.add_argument("--provider")
    for key in PRICES:
        renderer.add_argument("--price-" + key.replace("_", "-"), type=float, required=True)
    args = parser.parse_args()
    try:
        if args.command == "check":
            check(args.path)
        else:
            render(args.out, args.model, os.environ.get("PRX_BENCH_API_KEY", ""),
                   {key: getattr(args, "price_" + key) for key in PRICES}, args.provider)
    except (ValueError, OSError, tomllib.TOMLDecodeError):
        # Parser diagnostics may include config source containing a secret.
        print("invalid benchmark profile", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
