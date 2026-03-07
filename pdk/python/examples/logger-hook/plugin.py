"""
logger-hook — A PRX Hook plugin that logs lifecycle events and tracks counts.

This hook listens to all lifecycle events emitted by the PRX agent and:
  1. Logs each event at INFO level.
  2. Increments a KV counter per event type.
  3. Publishes a summary event on ``prx.audit.event_received``.

Build:
    componentize-py --wit-path ../../wit --world hook componentize plugin.py -o plugin.wasm

Install:
    Copy plugin.wasm and plugin.toml into the PRX plugins/ directory.

Local test:
    python -c "
    from plugin import on_event
    on_event('tool_call', '{\"tool\":\"base64\",\"args\":{}}')
    "
"""

from __future__ import annotations

import json

from prx_pdk import host, prx_hook


@prx_hook(events=["agent_start", "agent_stop", "tool_call", "tool_result", "error"])
def on_event(event: str, payload: dict) -> None:
    """Log every lifecycle event and update KV counters."""

    # 1. Log the event
    host.log.info(f"logger-hook: received event '{event}'")

    # 2. Increment per-event counter in KV store
    counter_key = f"event_count:{event}"
    count = host.kv.increment(counter_key, 1)
    host.log.debug(f"logger-hook: {event} count = {count}")

    # 3. Publish audit event (requires "events" permission)
    try:
        host.events.publish_json(
            "prx.audit.event_received",
            {
                "event": event,
                "count": count,
                "payload_keys": list(payload.keys()) if isinstance(payload, dict) else [],
                "ts_ms": host.clock.now_ms(),
            },
        )
    except RuntimeError as exc:
        # Events permission may not be granted — log and continue.
        host.log.warn(f"logger-hook: could not publish audit event: {exc}")


# ── Local smoke-test ───────────────────────────────────────────────────────────

if __name__ == "__main__":
    sample_payload = json.dumps({"tool": "base64", "args": {"input": "hello"}})
    on_event("tool_call", sample_payload)
    on_event("tool_call", sample_payload)
    on_event("agent_start", "{}")
