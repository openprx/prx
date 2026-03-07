"""
prx_pdk — PRX WASM Plugin Development Kit for Python.

Build PRX plugins with Python ≥ 3.10 and
`componentize-py <https://github.com/bytecodealliance/componentize-py>`_.

Quick start::

    from prx_pdk import prx_tool, ToolResult, host
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
        host.log.info("json_formatter called")
        data = json.loads(args["json_str"])
        return ToolResult.ok(json.dumps(data, indent=args.get("indent", 2)))

Build::

    componentize-py --wit-path ../../wit --world tool componentize plugin.py -o plugin.wasm
"""

from .decorators import (
    MiddlewareAction,
    ToolResult,
    prx_cron,
    prx_hook,
    prx_middleware,
    prx_tool,
)
from .types import (
    CronContext,
    HttpResponse,
    MemoryEntry,
    PluginResult,
    ToolSpec,
)
from . import host

__all__ = [
    # Decorators
    "prx_tool",
    "prx_hook",
    "prx_middleware",
    "prx_cron",
    # Types
    "ToolSpec",
    "PluginResult",
    "ToolResult",          # alias for PluginResult
    "MiddlewareAction",
    "HttpResponse",
    "MemoryEntry",
    "CronContext",
    # Host module
    "host",
]

__version__ = "0.1.0"
