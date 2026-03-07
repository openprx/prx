"""
prx_pdk.types — Core data types for PRX WASM plugins.

All types are aligned with the WIT definitions in wit/plugin/ and wit/host/,
and mirror the Rust PDK types in pdk/rust/prx-pdk/src/lib.rs.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Optional


# ── Tool types ────────────────────────────────────────────────────────────────

@dataclass
class ToolSpec:
    """Tool specification returned from ``get_spec``.

    Corresponds to the WIT record ``tool-spec`` in ``wit/plugin/tool.wit``.
    """

    name: str
    """Tool name (snake_case, matches the WIT record field)."""

    description: str
    """Human-readable description shown to the LLM."""

    parameters_schema: str
    """JSON Schema string describing the tool's input parameters."""

    def to_wit(self) -> dict:
        """Serialize to the dict expected by componentize-py bindings."""
        return {
            "name": self.name,
            "description": self.description,
            "parameters_schema": self.parameters_schema,
        }

    @classmethod
    def from_wit(cls, d: dict) -> "ToolSpec":
        return cls(
            name=d["name"],
            description=d["description"],
            parameters_schema=d["parameters_schema"],
        )


@dataclass
class PluginResult:
    """Result returned from plugin ``execute`` / ``run`` calls.

    Corresponds to the WIT record ``plugin-result`` in ``wit/plugin/tool.wit``.
    """

    success: bool
    """Whether the operation succeeded."""

    output: str
    """Output text (may be empty on error)."""

    error: Optional[str] = None
    """Optional error message (populated when ``success == False``)."""

    # ── Convenience constructors ───────────────────────────────────────────────

    @classmethod
    def ok(cls, output: str) -> "PluginResult":
        """Create a successful result."""
        return cls(success=True, output=output, error=None)

    @classmethod
    def err(cls, error: str) -> "PluginResult":
        """Create a failure result."""
        return cls(success=False, output="", error=error)

    def to_wit(self) -> dict:
        return {"success": self.success, "output": self.output, "error": self.error}

    @classmethod
    def from_wit(cls, d: dict) -> "PluginResult":
        return cls(success=d["success"], output=d["output"], error=d.get("error"))


# ── Middleware types ──────────────────────────────────────────────────────────

class MiddlewareAction:
    """Action returned by middleware plugins.

    Use the class-methods :meth:`continue_` and :meth:`block` to construct
    instances — do not instantiate directly.
    """

    __slots__ = ("_kind", "_data")

    def __init__(self, kind: str, data: str) -> None:
        self._kind = kind
        self._data = data

    # ── Factory methods ────────────────────────────────────────────────────────

    @classmethod
    def continue_(cls, data: str) -> "MiddlewareAction":
        """Pass the (possibly modified) data downstream."""
        return cls("continue", data)

    @classmethod
    def block(cls, reason: str) -> "MiddlewareAction":
        """Block the request with an error reason."""
        return cls("block", reason)

    # ── Properties ────────────────────────────────────────────────────────────

    @property
    def kind(self) -> str:
        """Either ``'continue'`` or ``'block'``."""
        return self._kind

    @property
    def data(self) -> str:
        """Modified data (if ``continue``) or reason (if ``block``)."""
        return self._data

    def is_continue(self) -> bool:
        return self._kind == "continue"

    def is_block(self) -> bool:
        return self._kind == "block"

    def to_wit(self) -> str:
        """Serialize to a JSON string for the WIT ``result<string, string>`` return.

        On ``continue``: returns the data string (passed to next stage).
        The calling decorator raises a Python exception for ``block``.
        """
        return self._data

    def __repr__(self) -> str:
        return f"MiddlewareAction.{self._kind}({self._data!r})"


# ── HTTP types ────────────────────────────────────────────────────────────────

@dataclass
class HttpResponse:
    """HTTP response from an outbound request.

    Corresponds to the WIT record ``http-response`` in ``wit/host/http.wit``.
    """

    status: int
    """HTTP status code."""

    headers: list[tuple[str, str]] = field(default_factory=list)
    """Response headers as ``(name, value)`` pairs."""

    body: bytes = field(default_factory=bytes)
    """Response body bytes."""

    def text(self) -> str:
        """Decode the body as UTF-8 text."""
        return self.body.decode("utf-8")

    def json(self) -> object:
        """Parse the body as JSON."""
        return json.loads(self.body)

    def header(self, name: str) -> Optional[str]:
        """Return the first header with the given name (case-insensitive)."""
        name_lower = name.lower()
        for k, v in self.headers:
            if k.lower() == name_lower:
                return v
        return None


# ── Memory types ──────────────────────────────────────────────────────────────

@dataclass
class MemoryEntry:
    """A memory entry returned from ``memory.recall``.

    Corresponds to the WIT record in ``wit/host/memory.wit``.
    """

    id: str
    """Unique entry ID."""

    text: str
    """Stored text content."""

    category: str
    """Category label (e.g. ``'fact'``, ``'preference'``)."""

    importance: float
    """Importance score (0.0–1.0)."""


# ── Cron types ────────────────────────────────────────────────────────────────

@dataclass
class CronContext:
    """Context passed to cron plugin ``run`` implementations.

    The host calls ``run()`` with no arguments per the WIT definition, but
    the Python PDK injects this context object to provide useful metadata.
    """

    triggered_at_ms: int = 0
    """Unix timestamp (milliseconds) when the cron job was triggered."""

    plugin_name: str = ""
    """Name of the plugin as declared in ``plugin.toml``."""
