"""
prx_pdk.decorators — Decorators for PRX WASM plugin authors.

These decorators register your Python functions as PRX plugin entry-points,
generate the WIT-compatible ``get_spec`` / ``execute`` / ``on_event`` /
``process`` / ``run`` exports, and handle JSON (de)serialisation.

Usage example::

    from prx_pdk import prx_tool, ToolResult
    import json

    @prx_tool(
        name="json_formatter",
        description="Format JSON with indentation",
        params={
            "type": "object",
            "properties": {
                "json_str": {"type": "string"},
                "indent":   {"type": "integer", "default": 2},
            },
            "required": ["json_str"],
        },
    )
    def execute(args: dict) -> ToolResult:
        data = json.loads(args["json_str"])
        return ToolResult.ok(json.dumps(data, indent=args.get("indent", 2)))
"""

from __future__ import annotations

import functools
import json
from typing import Any, Callable, Optional, Union

from .types import MiddlewareAction, PluginResult, ToolSpec

# Alias for user-facing convenience
ToolResult = PluginResult


# ── Internal registry ─────────────────────────────────────────────────────────
# componentize-py discovers exports via module-level names.  The decorators
# store the decorated functions here so that the generated WIT glue code can
# find them.

_registered_tool_execute: Optional[Callable] = None
_registered_tool_spec: Optional[ToolSpec] = None

_registered_hook_handler: Optional[Callable] = None
_registered_hook_events: list[str] = []

_registered_middleware_handler: Optional[Callable] = None
_registered_middleware_priority: int = 50

_registered_cron_handler: Optional[Callable] = None


# ── @prx_tool ─────────────────────────────────────────────────────────────────

def prx_tool(
    *,
    name: str,
    description: str,
    params: Union[dict, str],
) -> Callable:
    """Decorator: mark a function as a PRX Tool plugin entry-point.

    The decorated function receives the tool arguments as a ``dict`` and must
    return a :class:`~prx_pdk.types.PluginResult` (aliased as ``ToolResult``).

    :param name: Tool name (snake_case).  Shown to the LLM.
    :param description: Human-readable description.  Shown to the LLM.
    :param params: JSON Schema for the input parameters — either a Python dict
        (auto-serialised) or a pre-serialised JSON string.

    Example::

        @prx_tool(
            name="hello",
            description="Say hello",
            params={"type": "object", "properties": {"name": {"type": "string"}}},
        )
        def execute(args: dict) -> ToolResult:
            return ToolResult.ok(f"Hello, {args.get('name', 'world')}!")
    """
    global _registered_tool_execute, _registered_tool_spec

    if isinstance(params, dict):
        params_str = json.dumps(params)
    else:
        params_str = params

    spec = ToolSpec(name=name, description=description, parameters_schema=params_str)

    def decorator(fn: Callable) -> Callable:
        global _registered_tool_execute, _registered_tool_spec
        _registered_tool_spec = spec
        _registered_tool_execute = fn

        @functools.wraps(fn)
        def wrapper(args_json: str) -> dict:
            """WIT-compatible wrapper: accepts JSON string, returns WIT dict."""
            try:
                args = json.loads(args_json)
            except json.JSONDecodeError as exc:
                return PluginResult.err(f"Invalid JSON args: {exc}").to_wit()
            try:
                result = fn(args)
                if isinstance(result, PluginResult):
                    return result.to_wit()
                # Bare string return → wrap in PluginResult.ok
                return PluginResult.ok(str(result)).to_wit()
            except Exception as exc:  # noqa: BLE001
                return PluginResult.err(str(exc)).to_wit()

        # Attach WIT-facing helpers to the wrapper
        wrapper.get_spec = lambda: spec.to_wit()   # type: ignore[attr-defined]
        wrapper.execute = wrapper                   # type: ignore[attr-defined]
        return wrapper

    return decorator


# ── @prx_hook ────────────────────────────────────────────────────────────────

def prx_hook(*, events: Optional[list[str]] = None) -> Callable:
    """Decorator: mark a function as a PRX Hook plugin entry-point.

    The decorated function receives the event name and payload dict, and should
    return ``None`` on success or raise an exception on failure.

    :param events: Optional list of event names to subscribe to
        (e.g. ``["tool_call", "agent_start"]``).  When omitted, the hook
        receives all events.

    Example::

        @prx_hook(events=["tool_call", "agent_start"])
        def on_event(event: str, payload: dict) -> None:
            host.log.info(f"Got event: {event}")
    """
    global _registered_hook_handler, _registered_hook_events

    subscribed = list(events) if events else []

    def decorator(fn: Callable) -> Callable:
        global _registered_hook_handler, _registered_hook_events
        _registered_hook_events = subscribed
        _registered_hook_handler = fn

        @functools.wraps(fn)
        def wrapper(event: str, payload_json: str) -> None:
            """WIT-compatible wrapper: ``on-event(event, payload-json) -> result<_, string>``."""
            # Filter by subscribed events if specified
            if subscribed and event not in subscribed:
                return
            try:
                payload: Any = json.loads(payload_json) if payload_json else {}
            except json.JSONDecodeError:
                payload = payload_json  # pass raw string if not valid JSON
            fn(event, payload)

        wrapper.on_event = wrapper    # type: ignore[attr-defined]
        wrapper.subscribed_events = subscribed  # type: ignore[attr-defined]
        return wrapper

    return decorator


# ── @prx_middleware ───────────────────────────────────────────────────────────

def prx_middleware(*, priority: int = 50) -> Callable:
    """Decorator: mark a function as a PRX Middleware plugin entry-point.

    The decorated function receives the pipeline stage name and a data dict,
    and must return a :class:`~prx_pdk.types.MiddlewareAction`.

    :param priority: Execution order within the middleware chain (lower = earlier).
        Default is 50.

    Supported stages: ``"inbound"``, ``"outbound"``, ``"llm_request"``,
    ``"llm_response"``.

    Example::

        @prx_middleware(priority=10)
        def process(stage: str, data: dict) -> MiddlewareAction:
            if stage == "inbound":
                data["enriched"] = True
            return MiddlewareAction.continue_(json.dumps(data))
    """
    global _registered_middleware_handler, _registered_middleware_priority

    def decorator(fn: Callable) -> Callable:
        global _registered_middleware_handler, _registered_middleware_priority
        _registered_middleware_priority = priority
        _registered_middleware_handler = fn

        @functools.wraps(fn)
        def wrapper(stage: str, data_json: str) -> str:
            """WIT-compatible wrapper: ``process(stage, data-json) -> result<string, string>``."""
            try:
                data: Any = json.loads(data_json) if data_json else {}
            except json.JSONDecodeError:
                data = data_json
            result = fn(stage, data)
            if isinstance(result, MiddlewareAction):
                if result.is_block():
                    raise RuntimeError(result.data)  # WIT result<string, string> Err variant
                return result.to_wit()
            # Bare string return → pass through
            return str(result)

        wrapper.process = wrapper          # type: ignore[attr-defined]
        wrapper.priority = priority        # type: ignore[attr-defined]
        return wrapper

    return decorator


# ── @prx_cron ────────────────────────────────────────────────────────────────

def prx_cron(fn: Callable) -> Callable:
    """Decorator: mark a function as a PRX Cron plugin entry-point.

    The decorated function takes no arguments and must return a string message
    (logged by the host) or raise an exception on failure.

    Example::

        @prx_cron
        def run() -> str:
            host.log.info("Cron job running")
            return "completed"
    """
    global _registered_cron_handler
    _registered_cron_handler = fn

    @functools.wraps(fn)
    def wrapper() -> str:
        """WIT-compatible wrapper: ``run() -> result<string, string>``."""
        result = fn()
        return str(result) if result is not None else "ok"

    wrapper.run = wrapper    # type: ignore[attr-defined]
    return wrapper
