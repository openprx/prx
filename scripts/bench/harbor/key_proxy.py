"""Host-side forwarding proxy that keeps the paid provider key out of the task container.

The adapter starts one proxy per trial. The container's PRX profile points at
the proxy and holds only a per-trial random bearer token; the proxy swaps that
token for the real key and forwards the allowlisted inference calls to one
fixed HTTPS upstream. Standard library only. Nothing here logs or returns
headers, bodies or credentials, and the real key is scrubbed from everything
sent back.

Exposure: the listener is reachable by anything that can reach its bind
address. The adapter cannot learn the container network's addresses (Harbor's
compose file creates the network), so by default the token is the only gate.
`allowed_peers` adds a source-address filter once the operator knows the
address container traffic arrives from.
"""
import hmac
import http.client
import http.server
import ipaddress
import logging
import secrets
import socket
import ssl
import threading
import time
from urllib.parse import urlsplit

MAX_BODY = 32 * 1024 * 1024
MAX_INFLIGHT = 16
CLIENT_TIMEOUT = 60      # inactivity, client socket
CONNECT_TIMEOUT = 10     # upstream connect and TLS handshake
UPSTREAM_TIMEOUT = 600   # inactivity, upstream socket
READ_DEADLINE = 120      # absolute: request head and body fully received
REQUEST_DEADLINE = 1800  # absolute: whole exchange, upstream response included
CHUNK = 65536
CONTAINER_HOST = "host.containers.internal"
MARK = b"[redacted]"
# Everything the token may do, as (method, path below the upstream prefix).
# Derived from what PRX's OpenAI-compatible provider (src/providers/compatible.rs)
# sends for the supported providers (Kimi Code, DeepInfra, other custom: URLs).
ALLOWED = (
    # chat_completions_url(): every generation, streamed or not.
    ("POST", "/chat/completions"),
    # Not listed on purpose:
    # - GET /chat/completions (warmup): PRX ignores its status, so a 403 is harmless.
    # - POST /responses, /v1/responses (fallback after a chat 404): the supported
    #   providers serve chat completions, and that API can start background work
    #   that outlives the trial.
    # - GET /models (prx models refresh, prx providers test --live): never called
    #   by `prx agent` during a trial.
)
# Allowlists: nothing else crosses the proxy in either direction.
REQUEST_HEADERS = ("content-type", "accept", "user-agent")
RESPONSE_HEADERS = ("content-type", "retry-after", "cache-control")
REASONS = {400: "Bad Request", 401: "Unauthorized", 403: "Forbidden", 411: "Length Required",
           413: "Content Too Large", 502: "Bad Gateway", 503: "Service Unavailable"}
LOG = logging.getLogger("prx_bench.key_proxy")


def primary_ipv4():
    """Return the host's primary non-loopback IPv4 address without sending a packet."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        # TEST-NET-1: connect() on UDP only selects the outbound route.
        probe.connect(("192.0.2.1", 9))
        address = probe.getsockname()[0]
    if address.startswith("127.") or address == "0.0.0.0":
        raise OSError("no non-loopback IPv4 address; pass proxy_bind explicitly")
    return address


def holds_key(data, key):
    """True when data contains the key or its 8-byte prefix."""
    raw = key.encode()
    return any(token and token in data for token in (raw, raw[:8]))


def _printable(value):
    return all(0x21 <= ord(char) <= 0x7e for char in value)


class _Stopped(OSError):
    """The proxy was stopped while this exchange was in flight."""


class Scrubber:
    """Replace the key, and any run of 8 or more of its leading bytes, in a byte stream.

    A tail that could be the start of the key is held back until the next
    chunk decides, so a key split across chunks never leaves in pieces. Only
    such a tail is delayed (at most key length minus one bytes).
    """

    def __init__(self, key):
        self.key = key
        self.short = key[:8]
        self.held = b""

    def _runs(self, data):
        out = bytearray()
        start = 0
        while True:
            found = data.find(self.short, start)
            if found < 0:
                out += data[start:]
                return bytes(out)
            size = len(self.short)
            while found + size < len(data) and size < len(self.key) and data[found + size] == self.key[size]:
                size += 1
            out += data[start:found]
            out += MARK
            start = found + size

    def feed(self, data):
        buffer = self.held + data
        keep = 0
        limit = min(len(buffer), len(self.key) - 1)
        if limit and self.key[:1] in buffer[-limit:]:
            for size in range(limit, 0, -1):
                if buffer.endswith(self.key[:size]):
                    keep = size
                    break
        self.held = buffer[len(buffer) - keep:]
        return self._runs(buffer[:len(buffer) - keep])

    def flush(self):
        out, self.held = self._runs(self.held), b""
        return out


class _Exchange:
    """One accepted client connection and, later, its upstream connection."""

    def __init__(self, client, deadline):
        self.client = client
        self.started = time.monotonic()
        self.deadline = deadline
        self.upstream = None       # http.client connection, registered before connect()
        self.upstream_sock = None  # kept separately: http.client detaches it on Connection: close
        self.thread = None

    def cut(self):
        for sock in (self.client, self.upstream_sock):
            if sock is not None:
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = CLIENT_TIMEOUT
    server_version = "prx-bench-key-proxy"
    sys_version = ""

    def log_message(self, format, *args):
        # The stdlib access and error logs can carry request lines; keep them off.
        del format, args

    def _serve(self):
        proxy = self.server.proxy
        self.exchange = proxy.exchange_for(self.request)
        self.close_connection = True
        self.head_sent = False
        self.status = 0
        self.received = 0
        self.sent = 0
        try:
            if self.exchange is None:
                raise _Stopped()
            self._relay(proxy)
        except (OSError, http.client.HTTPException, ValueError):
            # Exception text may quote header values; report a bare status only.
            # After the head went out the only honest signal is a cut connection.
            if not self.head_sent:
                try:
                    self._head(503 if proxy.stopped.is_set() else 502)
                except OSError:
                    self.status = self.status or 502
        finally:
            proxy.log(self.command, self.path, self.status, self.received, self.sent)

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = do_OPTIONS = _serve

    def _head(self, status, headers=(), reason=None):
        """Write the one status line and header block of this response."""
        lines = [f"HTTP/1.1 {status} {reason or REASONS.get(status, 'Upstream')}"]
        lines += [f"{name}: {value}" for name, value in headers]
        if not any(name == "Transfer-Encoding" for name, _ in headers):
            lines.append("Content-Length: 0")
        lines.append("Connection: close")
        self.head_sent = True
        self.status = status
        self.wfile.write(("\r\n".join(lines) + "\r\n\r\n").encode("latin-1", "replace"))
        self.wfile.flush()

    def _relay(self, proxy):
        if not proxy.authorized(self.headers.get("Authorization")):
            return self._head(401)
        if proxy.stopped.is_set():
            return self._head(503)
        if not proxy.call_allowed(self.command, self.path):
            return self._head(403)
        if self.headers.get("Transfer-Encoding") is not None:
            return self._head(411)
        declared = self.headers.get("Content-Length")
        if declared is None:
            return self._head(411)
        if not declared.isascii() or not declared.isdigit() or len(declared) > 12:
            return self._head(400)
        length = int(declared)
        if length > proxy.max_body:
            return self._head(413)
        body = self.rfile.read(length) if length else b""
        self.received = len(body)
        if len(body) != length:
            return self._head(400)
        # The request is in: from here the whole-exchange deadline applies.
        self.exchange.deadline = self.exchange.started + proxy.request_deadline
        connection = proxy.connect(self.exchange)
        connection.putrequest(self.command, self.path, skip_accept_encoding=True)
        for name in REQUEST_HEADERS:
            value = self.headers.get(name)
            if value is not None:
                connection.putheader(name, value)
        connection.putheader("Accept-Encoding", "identity")
        connection.putheader("Authorization", "Bearer " + proxy.key)
        connection.putheader("Content-Length", str(length))
        connection.endheaders(body or None)
        self._respond(proxy, connection.getresponse())

    def _respond(self, proxy, response):
        encoding = response.getheader("Content-Encoding")
        if encoding is not None and encoding.strip().lower() != "identity":
            raise ValueError("encoded upstream body cannot be scrubbed")
        headers = []
        for name in RESPONSE_HEADERS:
            value = response.getheader(name)
            if value is None:
                continue
            value = proxy.redact(value.encode("latin-1", "replace")).decode("latin-1")
            if "\r" not in value and "\n" not in value:
                headers.append((name.title(), value))
        if response.status in (204, 304):
            return self._head(response.status, headers, "Upstream")
        headers.append(("Transfer-Encoding", "chunked"))
        self._head(response.status, headers, "Upstream")
        # Stream as bytes arrive so SSE and chunked replies are never buffered
        # whole; success and error bodies alike pass through the scrubber.
        scrubber = Scrubber(proxy.key.encode())
        while True:
            data = response.read1(CHUNK)
            if proxy.stopped.is_set():
                raise _Stopped()
            out = scrubber.feed(data) if data else scrubber.flush()
            if out:
                self.wfile.write(b"%x\r\n%b\r\n" % (len(out), out))
                self.wfile.flush()
                self.sent += len(out)
            if not data:
                break
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


class _Server(http.server.HTTPServer):
    request_queue_size = 32

    def process_request(self, request, client_address):
        # Admission happens before a single byte is read from the client.
        exchange = self.proxy.admit(request, client_address)
        if exchange is None:
            self.shutdown_request(request)
            return
        exchange.thread = threading.Thread(target=self._work, args=(request, client_address, exchange), daemon=True)
        try:
            exchange.thread.start()
        except RuntimeError:
            exchange.thread = None
            self.shutdown_request(request)
            self.proxy.release(exchange)

    def _work(self, request, client_address, exchange):
        try:
            self.finish_request(request, client_address)
        except Exception:  # same boundary as socketserver: one bad exchange must not print or propagate
            pass
        finally:
            self.shutdown_request(request)
            self.proxy.release(exchange)

    def service_actions(self):
        self.proxy.reap()

    def handle_error(self, request, client_address):
        # The default prints a traceback, which may include request data.
        del request, client_address


class KeyProxy:
    """One trial's forwarding proxy; `token` is the only credential the container sees."""

    def __init__(self, upstream, key, bind, port=0, *, allow_http_upstream=False, allowed_peers=None,
                 upstream_timeout=UPSTREAM_TIMEOUT, max_body=MAX_BODY, max_inflight=MAX_INFLIGHT,
                 read_deadline=READ_DEADLINE, request_deadline=REQUEST_DEADLINE):
        parts = urlsplit(upstream)
        schemes = ("https", "http") if allow_http_upstream else ("https",)
        if parts.scheme not in schemes:
            raise ValueError("proxy upstream must be an https URL")
        if not parts.hostname or parts.username or parts.password or parts.query or parts.fragment:
            raise ValueError("proxy upstream must be a plain base URL")
        prefix = parts.path.rstrip("/")
        if (not prefix.startswith("/") or not _printable(prefix) or "\\" in prefix or "%" in prefix
                or any(segment in (".", "..") for segment in prefix.split("/"))):
            raise ValueError("proxy upstream needs a plain, non-empty API path prefix")
        if not key or not _printable(key):
            # Never echo the value: http.client would quote it in its own error.
            raise ValueError("key file content is not a usable bearer credential")
        self.peers = None
        if allowed_peers is not None:
            self.peers = [ipaddress.ip_network(peer.strip(), strict=False) for peer in allowed_peers]
        self.scheme, self.host, self.prefix = parts.scheme, parts.hostname, prefix
        self.port = parts.port or (443 if parts.scheme == "https" else 80)
        self.key = key
        self.token = secrets.token_urlsafe(32)
        self.max_body = max_body
        self.max_inflight = max_inflight
        self.upstream_timeout = upstream_timeout
        self.read_deadline = read_deadline
        self.request_deadline = request_deadline
        self.tls = ssl.create_default_context() if parts.scheme == "https" else None
        self.stopped = threading.Event()
        self._lock = threading.Lock()
        self._exchanges = {}
        self._server = _Server((bind, port), _Handler)
        self._server.proxy = self
        self._thread = threading.Thread(target=self._server.serve_forever, args=(0.05,), daemon=True)
        self._started = False

    @property
    def address(self):
        return self._server.server_address[:2]

    def container_base_url(self):
        return f"http://{CONTAINER_HOST}:{self.address[1]}{self.prefix}"

    def start(self):
        self._started = True
        self._thread.start()

    def admit(self, request, client_address):
        """Register a new connection, or return None to drop it unread."""
        if self.peers is not None:
            try:
                peer = ipaddress.ip_address(client_address[0])
            except ValueError:
                return None
            if not any(peer in network for network in self.peers):
                return None
        with self._lock:
            if self.stopped.is_set() or len(self._exchanges) >= self.max_inflight:
                return None
            exchange = _Exchange(request, time.monotonic() + self.read_deadline)
            self._exchanges[id(request)] = exchange
            return exchange

    def exchange_for(self, request):
        with self._lock:
            return self._exchanges.get(id(request))

    def release(self, exchange):
        with self._lock:
            self._exchanges.pop(id(exchange.client), None)
            connection, exchange.upstream = exchange.upstream, None
        if connection is not None:
            connection.close()
        if exchange.upstream_sock is not None:
            try:
                exchange.upstream_sock.close()
            except OSError:
                pass

    def reap(self):
        """Cut every exchange past its absolute deadline; called from the accept loop."""
        now = time.monotonic()
        with self._lock:
            late = [exchange for exchange in self._exchanges.values() if now >= exchange.deadline]
        for exchange in late:
            exchange.cut()

    def stop(self, join_timeout=CONNECT_TIMEOUT + 2):
        """Stop serving and cut every exchange; the token is dead afterwards.

        Returns True when no handler thread is left. After this call no
        handler can open an upstream connection or send on an existing one:
        new connections are refused under the same lock that sets the flag,
        every registered socket is shut down, and a connection still inside
        connect() is closed by its own thread before it sends anything.
        """
        with self._lock:
            already = self.stopped.is_set()
            self.stopped.set()
            exchanges = list(self._exchanges.values())
        if not already:
            if self._started:
                self._server.shutdown()
            self._server.server_close()
        for exchange in exchanges:
            exchange.cut()
        deadline = time.monotonic() + join_timeout
        threads = [exchange.thread for exchange in exchanges if exchange.thread is not None]
        if self._started:
            threads.append(self._thread)
        for thread in threads:
            if thread is not threading.current_thread():
                thread.join(max(0.0, deadline - time.monotonic()))
        return not any(thread.is_alive() for thread in threads)

    def authorized(self, header):
        if not header or not header.startswith("Bearer "):
            return False
        return hmac.compare_digest(header[7:].strip().encode("latin-1", "replace"), self.token.encode())

    def call_allowed(self, method, target):
        """Exact (method, path) pairs only: no query, no other path, and the host is never taken from the request."""
        return any(method == allowed and target == self.prefix + suffix for allowed, suffix in ALLOWED)

    def connect(self, exchange):
        """Open the upstream connection for one exchange, never after stop()."""
        if self.scheme == "https":
            connection = http.client.HTTPSConnection(self.host, self.port, timeout=CONNECT_TIMEOUT, context=self.tls)
        else:
            connection = http.client.HTTPConnection(self.host, self.port, timeout=CONNECT_TIMEOUT)
        with self._lock:
            if self.stopped.is_set():
                raise _Stopped()
            exchange.upstream = connection
        try:
            connection.connect()
        except BaseException:
            with self._lock:
                exchange.upstream = None
            connection.close()
            raise
        with self._lock:
            exchange.upstream_sock = connection.sock
            stopped = self.stopped.is_set()
        if stopped:
            # stop() ran while connect() was in flight and could not see this socket.
            exchange.cut()
            raise _Stopped()
        connection.sock.settimeout(self.upstream_timeout)
        return connection

    def redact(self, data):
        scrubber = Scrubber(self.key.encode())
        return scrubber.feed(data) + scrubber.flush()

    def log(self, method, target, status, received, sent):
        # Method, path, status and byte counts only: no query, headers or bodies.
        path = "".join(char if 0x21 <= ord(char) <= 0x7e else "?" for char in str(target).split("?", 1)[0][:200])
        LOG.info("%s %s %s in=%d out=%d", method, path, status, received, sent)
