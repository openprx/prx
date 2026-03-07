"""
prx_pdk.host — Host function wrappers for PRX WASM plugins.

When running inside a WASM component (built with componentize-py), these
functions call the real host via WIT-generated bindings.

When imported in a regular Python environment (local development / testing),
they fall back to safe stubs so that plugin code can be unit-tested without
a WASM runtime.

Usage::

    from prx_pdk import host

    host.log.info("Plugin started")
    value = host.config.get("timeout_ms")
    host.kv.set("last_run", b"2025-01-01")
"""

from __future__ import annotations

import json
import os
import sys
import time
from typing import Optional

from .types import HttpResponse, MemoryEntry

# ── Detect WASM environment ───────────────────────────────────────────────────
# componentize-py sets sys.platform == "wasi" and generates the
# `prx.host.*` package as WIT bindings.

_WASM = sys.platform == "wasi"

if _WASM:
    try:
        from prx.host import log as _wit_log          # type: ignore[import]
        from prx.host import config as _wit_config    # type: ignore[import]
        from prx.host import kv as _wit_kv            # type: ignore[import]
        from prx.host import events as _wit_events    # type: ignore[import]
        from prx.host import http_outbound as _wit_http  # type: ignore[import]
        from prx.host import memory as _wit_memory    # type: ignore[import]
        _BINDINGS_AVAILABLE = True
    except ImportError:
        _BINDINGS_AVAILABLE = False
else:
    _BINDINGS_AVAILABLE = False


# ── log ───────────────────────────────────────────────────────────────────────

class _Log:
    """Structured logging — writes to the PRX tracing infrastructure.

    Log messages appear in the host's log output with the plugin name as
    context.

    WIT interface: ``prx:host/log``
    """

    def _emit(self, level: str, msg: str) -> None:
        if _BINDINGS_AVAILABLE:
            _wit_log.log(level, msg)
        else:
            tag = level.upper().ljust(5)
            print(f"[prx-pdk {tag}] {msg}", file=sys.stderr)

    def info(self, msg: str) -> None:
        """Emit an INFO-level log message."""
        self._emit("info", msg)

    def warn(self, msg: str) -> None:
        """Emit a WARN-level log message."""
        self._emit("warn", msg)

    def error(self, msg: str) -> None:
        """Emit an ERROR-level log message."""
        self._emit("error", msg)

    def debug(self, msg: str) -> None:
        """Emit a DEBUG-level log message."""
        self._emit("debug", msg)

    def trace(self, msg: str) -> None:
        """Emit a TRACE-level log message."""
        self._emit("trace", msg)


log = _Log()


# ── config ────────────────────────────────────────────────────────────────────

class _Config:
    """Plugin configuration — read-only access to values from ``plugin.toml [config]``.

    Config values are set by the operator when deploying the plugin and cannot
    be modified at runtime.  Use :mod:`kv` for mutable persistent storage.

    WIT interface: ``prx:host/config``
    """

    def get(self, key: str) -> Optional[str]:
        """Get a configuration value by key. Returns ``None`` if not set."""
        if _BINDINGS_AVAILABLE:
            return _wit_config.get(key)
        # Local stub: fall back to environment variables
        return os.environ.get(key)

    def get_all(self) -> list[tuple[str, str]]:
        """Get all configuration key-value pairs."""
        if _BINDINGS_AVAILABLE:
            return list(_wit_config.get_all())
        return []

    def get_or(self, key: str, default: str) -> str:
        """Get a configuration value, returning *default* if not set."""
        v = self.get(key)
        return v if v is not None else default


config = _Config()


# ── kv ───────────────────────────────────────────────────────────────────────

class _Kv:
    """Key-value storage — isolated per-plugin persistent store.

    Each plugin gets its own namespace; plugins cannot access each other's keys.
    Values are raw bytes.  Use :meth:`get_json` / :meth:`set_json` for
    structured data.

    WIT interface: ``prx:host/kv``
    """

    # Local-dev in-memory store (only used when not in WASM)
    _store: dict[str, bytes] = {}

    def get(self, key: str) -> Optional[bytes]:
        """Retrieve a value by key. Returns ``None`` if the key does not exist."""
        if _BINDINGS_AVAILABLE:
            return _wit_kv.get(key)
        return self._store.get(key)

    def get_str(self, key: str) -> Optional[str]:
        """Retrieve and decode as UTF-8 text."""
        v = self.get(key)
        if v is None:
            return None
        return v.decode("utf-8")

    def get_json(self, key: str) -> object:
        """Retrieve and JSON-deserialise a stored value. Returns ``None`` if missing."""
        v = self.get(key)
        if v is None:
            return None
        return json.loads(v)

    def set(self, key: str, value: bytes) -> None:
        """Store a byte value. Overwrites any existing value."""
        if _BINDINGS_AVAILABLE:
            _wit_kv.set(key, value)
        else:
            self._store[key] = value

    def set_str(self, key: str, value: str) -> None:
        """Store a UTF-8 string value."""
        self.set(key, value.encode("utf-8"))

    def set_json(self, key: str, value: object) -> None:
        """JSON-serialise and store a value."""
        self.set(key, json.dumps(value).encode("utf-8"))

    def delete(self, key: str) -> bool:
        """Delete a key. Returns ``True`` if the key existed."""
        if _BINDINGS_AVAILABLE:
            return _wit_kv.delete(key)
        existed = key in self._store
        self._store.pop(key, None)
        return existed

    def list_keys(self, prefix: str) -> list[str]:
        """List all keys matching a prefix."""
        if _BINDINGS_AVAILABLE:
            return list(_wit_kv.list_keys(prefix))
        return [k for k in self._store if k.startswith(prefix)]

    def increment(self, key: str, delta: int = 1) -> int:
        """Atomically increment an integer counter stored at *key*.

        Initialises to 0 if the key does not exist, then adds *delta*.
        """
        current_bytes = self.get(key)
        if current_bytes is None:
            current = 0
        else:
            try:
                current = json.loads(current_bytes)
                if not isinstance(current, int):
                    raise ValueError(f"not an int: {current!r}")
            except (ValueError, json.JSONDecodeError) as exc:
                log.warn(f"kv.increment: key '{key}' exists but is not a valid int "
                         f"(resetting to 0): {exc}")
                current = 0
        next_val = current + delta
        self.set_json(key, next_val)
        return next_val


kv = _Kv()


# ── events ────────────────────────────────────────────────────────────────────

class _Events:
    """Event bus — fire-and-forget publish/subscribe for inter-plugin communication.

    Events flow through the host for auditing and access control.
    Payload must be valid JSON, max 64 KB.

    WIT interface: ``prx:host/events``
    """

    def publish(self, topic: str, payload: str) -> None:
        """Publish an event to a topic.

        All subscribers matching the topic will receive the event asynchronously.

        :raises RuntimeError: if the plugin lacks ``events`` permission or the
            payload exceeds 64 KB.
        """
        if _BINDINGS_AVAILABLE:
            result = _wit_events.publish(topic, payload)
            if isinstance(result, Exception):
                raise RuntimeError(str(result))
        else:
            log.debug(f"[stub] events.publish topic={topic!r} payload={payload!r}")

    def publish_json(self, topic: str, payload: object) -> None:
        """Publish a JSON-serialisable value to a topic."""
        self.publish(topic, json.dumps(payload))

    def subscribe(self, pattern: str) -> int:
        """Subscribe to a topic pattern.

        Supports exact match (``"weather.update"``) and wildcard
        (``"weather.*"``).  Returns a subscription ID for use with
        :meth:`unsubscribe`.
        """
        if _BINDINGS_AVAILABLE:
            return _wit_events.subscribe(pattern)
        log.debug(f"[stub] events.subscribe pattern={pattern!r}")
        return 0

    def unsubscribe(self, subscription_id: int) -> None:
        """Cancel a subscription by ID."""
        if _BINDINGS_AVAILABLE:
            _wit_events.unsubscribe(subscription_id)
        else:
            log.debug(f"[stub] events.unsubscribe id={subscription_id}")


events = _Events()


# ── http ──────────────────────────────────────────────────────────────────────

class _Http:
    """Outbound HTTP — make controlled HTTP requests from plugins.

    URLs are validated against the plugin's ``http_allowlist`` in ``plugin.toml``.
    Requires ``"http-outbound"`` permission.

    WIT interface: ``prx:host/http-outbound``
    """

    def request(
        self,
        method: str,
        url: str,
        headers: list[tuple[str, str]] | None = None,
        body: bytes | None = None,
    ) -> HttpResponse:
        """Make an HTTP request.

        :param method: HTTP verb (``"GET"``, ``"POST"``, etc.)
        :param url: Target URL (must be in the plugin's ``http_allowlist``)
        :param headers: Request headers as ``(name, value)`` pairs
        :param body: Optional request body bytes
        :raises RuntimeError: if the URL is not allowed, or the plugin lacks
            ``"http-outbound"`` permission.
        """
        if headers is None:
            headers = []
        if _BINDINGS_AVAILABLE:
            result = _wit_http.request(method, url, headers, body)
            return HttpResponse(
                status=result.status,
                headers=list(result.headers),
                body=bytes(result.body),
            )
        raise RuntimeError(
            "http.request is only available inside a WASM component. "
            "Use a mock in your tests."
        )

    def get(self, url: str, headers: list[tuple[str, str]] | None = None) -> HttpResponse:
        """Convenience: HTTP GET request."""
        return self.request("GET", url, headers or [])

    def post_json(
        self,
        url: str,
        payload: object,
        headers: list[tuple[str, str]] | None = None,
    ) -> HttpResponse:
        """Convenience: HTTP POST with a JSON body."""
        h = list(headers or [])
        if not any(k.lower() == "content-type" for k, _ in h):
            h.append(("Content-Type", "application/json"))
        return self.request("POST", url, h, json.dumps(payload).encode())


http = _Http()


# ── clock ─────────────────────────────────────────────────────────────────────

class _Clock:
    """Clock — current time utilities for plugins.

    Note: The PRX WIT spec does not currently expose a dedicated clock interface.
    In WASM, WASI time functions are used.  On the host, ``time.time()`` is used.
    """

    def now_ms(self) -> int:
        """Return the current time as Unix milliseconds (UTC)."""
        return int(time.time() * 1000)

    def timezone(self) -> str:
        """Return the host timezone name.

        Currently always returns ``"UTC"`` — timezone support is planned for a
        future PRX host interface release.
        """
        return "UTC"


clock = _Clock()


# ── memory ────────────────────────────────────────────────────────────────────

class _Memory:
    """Long-term memory — store and recall text entries.

    Requires ``"memory"`` permission in ``plugin.toml``.

    WIT interface: ``prx:host/memory``
    """

    def store(self, text: str, category: str) -> str:
        """Store text in memory. Returns the generated entry ID."""
        if _BINDINGS_AVAILABLE:
            return _wit_memory.store(text, category)
        log.debug(f"[stub] memory.store category={category!r} text={text!r}")
        return "stub-id"

    def recall(self, query: str, limit: int = 10) -> list[MemoryEntry]:
        """Recall memories matching a query. Returns up to *limit* entries."""
        if _BINDINGS_AVAILABLE:
            raw = _wit_memory.recall(query, limit)
            return [
                MemoryEntry(
                    id=e.id,
                    text=e.text,
                    category=e.category,
                    importance=e.importance,
                )
                for e in raw
            ]
        return []


memory = _Memory()
