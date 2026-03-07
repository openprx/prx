"""
hello-tool — A simple PRX Tool plugin that formats JSON.

Build:
    componentize-py --wit-path ../../wit --world tool componentize plugin.py -o plugin.wasm

Install:
    Copy plugin.wasm and plugin.toml into the PRX plugins/ directory.

Test locally (without WASM runtime):
    python -c "
    from plugin import execute
    import json
    result = execute({'json_str': '{\"a\":1,\"b\":2}', 'indent': 4})
    print(result)
    "
"""

from __future__ import annotations

import json

from prx_pdk import ToolResult, host, prx_tool


@prx_tool(
    name="json_formatter",
    description=(
        "Format a JSON string with configurable indentation. "
        "Useful for making compact JSON human-readable."
    ),
    params={
        "type": "object",
        "properties": {
            "json_str": {
                "type": "string",
                "description": "The compact JSON string to format.",
            },
            "indent": {
                "type": "integer",
                "description": "Number of spaces for indentation (default: 2).",
                "default": 2,
                "minimum": 0,
                "maximum": 8,
            },
        },
        "required": ["json_str"],
    },
)
def execute(args: dict) -> ToolResult:
    """Format a JSON string with the requested indentation."""
    host.log.info("json_formatter: received request")

    json_str = args.get("json_str", "")
    indent = int(args.get("indent", 2))

    if not json_str:
        return ToolResult.err("'json_str' argument is required and must not be empty")

    try:
        data = json.loads(json_str)
    except json.JSONDecodeError as exc:
        return ToolResult.err(f"Invalid JSON input: {exc}")

    formatted = json.dumps(data, indent=indent, ensure_ascii=False)
    host.log.info(f"json_formatter: formatted {len(json_str)} → {len(formatted)} bytes")
    return ToolResult.ok(formatted)


# ── Local development entry-point ─────────────────────────────────────────────

if __name__ == "__main__":
    # Quick smoke-test — not executed inside the WASM component.
    sample = '{"name":"Alice","age":30,"tags":["python","wasm"]}'
    result = execute({"json_str": sample, "indent": 2})
    print("success:", result["success"])
    print(result["output"])
