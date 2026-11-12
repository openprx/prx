"""Host-side forwarding proxy that keeps the paid provider key out of the task container.

The adapter starts one proxy per trial. The container's PRX profile points at
the proxy and holds only a per-trial random bearer token; the proxy swaps that
token for the real key and forwards to one fixed HTTPS upstream. Standard
library only. Nothing here logs or returns headers, bodies or credentials.
"""
import hmac
import http.client
import http.server
import logging
import secrets
import socket
import ssl
import threading
from urllib.parse import urlsplit

MAX_BODY = 32 * 1024 * 1024
MAX_ERROR_BODY = 64 * 1024
MAX_INFLIGHT = 16
CLIENT_TIMEOUT = 60
CONNECT_TIMEOUT = 10
UPSTREAM_TIMEOUT = 600
CHUNK = 65536
CONTAINER_HOST = "host.containers.internal"
# Allowlists: nothing else crosses the proxy in either direction.
REQUEST_HEADERS = ("content-type", "accept", "user-agent")
RESPONSE_HEADERS = ("content-type", "content-encoding", "retry-after", "cache-control")
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


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    timeout = CLIENT_TIMEOUT
    server_version = "prx-bench-key-proxy"
    sys_version = ""

    def log_message(self, format, *args):
        # The stdlib access and error logs can carry request lines; keep them off.
        del format, args

    def do_GET(self):
        self._serve()

    def do_POST(self):
        self._serve()

    def _serve(self):
        proxy = self.server.proxy
        self.close_connection = True
        self.head_sent = False
        self.status = 0
        self.received = 0
        self.sent = 0
        try:
            self._relay(proxy)
        except (OSError, http.client.HTTPException, ValueError):
            # Exception text may quote header values; report a bare status only.
            if not self.head_sent:
                try:
                    self._refuse(502)
                except OSError:
                    self.status = 502
        finally:
            proxy.log(self.command, self.path, self.status, self.received, self.sent)

    def _refuse(self, status):
        self.status = status
        self.send_response_only(status)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()
        self.head_sent = True

    def _relay(self, proxy):
        if not proxy.authorized(self.headers.get("Authorization")):
            return self._refuse(401)
        if proxy.stopped.is_set():
            return self._refuse(503)
        if not proxy.path_allowed(self.path):
            return self._refuse(404)
        if self.headers.get("Transfer-Encoding") is not None:
            return self._refuse(411)
        declared = self.headers.get("Content-Length")
        if declared is None:
            if self.command == "POST":
                return self._refuse(411)
            length = 0
        elif not declared.isascii() or not declared.isdigit() or len(declared) > 12:
            return self._refuse(400)
        else:
            length = int(declared)
        if length > proxy.max_body:
            return self._refuse(413)
        if self.command == "GET" and length:
            return self._refuse(400)
        if not proxy.slots.acquire(blocking=False):
            return self._refuse(503)
        connection = None
        try:
            body = self.rfile.read(length) if length else b""
            self.received = len(body)
            if len(body) != length:
                return self._refuse(400)
            connection = proxy.connect()
            connection.putrequest(self.command, self.path, skip_accept_encoding=True)
            for name in REQUEST_HEADERS:
                value = self.headers.get(name)
                if value is not None:
                    connection.putheader(name, value)
            connection.putheader("Accept-Encoding", "identity")
            connection.putheader("Authorization", "Bearer " + proxy.key)
            if self.command == "POST":
                connection.putheader("Content-Length", str(length))
            connection.endheaders(body or None)
            self._respond(proxy, connection.getresponse())
        finally:
            proxy.slots.release()
            if connection is not None:
                proxy.forget(connection)

    def _respond(self, proxy, response):
        self.status = response.status
        self.send_response_only(response.status)
        self.send_header("Connection", "close")
        if response.status >= 400:
            # Error bodies are small: bound them and scrub any echoed credential.
            encoded = response.getheader("Content-Encoding") is not None
            data = b"" if encoded else proxy.redact(response.read(MAX_ERROR_BODY))
            content_type = response.getheader("Content-Type")
            if content_type and not encoded:
                self.send_header("Content-Type", content_type)
            retry = response.getheader("Retry-After")
            if retry:
                self.send_header("Retry-After", retry)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.head_sent = True
            self._write(data)
            return
        for name in RESPONSE_HEADERS:
            value = response.getheader(name)
            if value is not None:
                self.send_header(name, value)
        if response.status in (204, 304):
            self.end_headers()
            self.head_sent = True
            return
        length = response.getheader("Content-Length")
        chunked = length is None or not length.isascii() or not length.isdigit()
        self.send_header(*(("Transfer-Encoding", "chunked") if chunked else ("Content-Length", length)))
        self.end_headers()
        self.head_sent = True
        # Stream as bytes arrive so SSE and chunked replies are never buffered whole.
        while True:
            data = response.read1(CHUNK)
            if not data:
                break
            self._write(b"%x\r\n%b\r\n" % (len(data), data) if chunked else data, len(data))
        if chunked:
            self._write(b"0\r\n\r\n", 0)

    def _write(self, data, counted=None):
        self.wfile.write(data)
        self.wfile.flush()
        self.sent += len(data) if counted is None else counted


class _Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 32

    def handle_error(self, request, client_address):
        # The default prints a traceback, which may include request data.
        del request, client_address


class KeyProxy:
    """One trial's forwarding proxy; `token` is the only credential the container sees."""

    def __init__(self, upstream, key, bind, port=0, *, allow_http_upstream=False,
                 upstream_timeout=UPSTREAM_TIMEOUT, max_body=MAX_BODY):
        parts = urlsplit(upstream)
        schemes = ("https", "http") if allow_http_upstream else ("https",)
        if parts.scheme not in schemes:
            raise ValueError("proxy upstream must be an https URL")
        if not parts.hostname or parts.username or parts.password or parts.query or parts.fragment:
            raise ValueError("proxy upstream must be a plain base URL")
        prefix = parts.path.rstrip("/")
        if not prefix.startswith("/") or not _printable(prefix) or not self._segments_safe(prefix):
            raise ValueError("proxy upstream needs a non-empty API path prefix")
        if not key or not _printable(key):
            # Never echo the value: http.client would quote it in its own error.
            raise ValueError("key file content is not a usable bearer credential")
        self.scheme, self.host, self.prefix = parts.scheme, parts.hostname, prefix
        self.port = parts.port or (443 if parts.scheme == "https" else 80)
        self.key = key
        self.token = secrets.token_urlsafe(32)
        self.max_body = max_body
        self.upstream_timeout = upstream_timeout
        self.tls = ssl.create_default_context() if parts.scheme == "https" else None
        self.slots = threading.BoundedSemaphore(MAX_INFLIGHT)
        self.stopped = threading.Event()
        self._lock = threading.Lock()
        self._active = set()
        self._server = _Server((bind, port), _Handler)
        self._server.proxy = self
        self._thread = threading.Thread(target=self._server.serve_forever, args=(0.1,), daemon=True)
        self._started = False

    @property
    def address(self):
        return self._server.server_address[:2]

    def container_base_url(self):
        return f"http://{CONTAINER_HOST}:{self.address[1]}{self.prefix}"

    def start(self):
        self._started = True
        self._thread.start()

    def stop(self):
        """Stop serving and cut in-flight exchanges; the token is dead afterwards."""
        if self.stopped.is_set():
            return
        self.stopped.set()
        if self._started:
            self._server.shutdown()
        self._server.server_close()
        with self._lock:
            active = list(self._active)
        for connection in active:
            try:
                if connection.sock is not None:
                    connection.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        if self._started:
            self._thread.join(timeout=2)

    def authorized(self, header):
        if not header or not header.startswith("Bearer "):
            return False
        return hmac.compare_digest(header[7:].strip().encode("latin-1", "replace"), self.token.encode())

    @staticmethod
    def _segments_safe(path):
        lowered = path.lower()
        if "\\" in path or any(code in lowered for code in ("%2e", "%2f", "%5c")):
            return False
        return not any(segment in (".", "..") for segment in path.split("/"))

    def path_allowed(self, target):
        """Origin-form targets under the upstream API prefix only; the host is never taken from the request."""
        if not target.startswith("/") or target.startswith("//") or not _printable(target) or "#" in target:
            return False
        path = target.split("?", 1)[0]
        if not self._segments_safe(path):
            return False
        return path == self.prefix or path.startswith(self.prefix + "/")

    def connect(self):
        if self.scheme == "https":
            connection = http.client.HTTPSConnection(self.host, self.port, timeout=CONNECT_TIMEOUT, context=self.tls)
        else:
            connection = http.client.HTTPConnection(self.host, self.port, timeout=CONNECT_TIMEOUT)
        with self._lock:
            self._active.add(connection)
        connection.connect()
        connection.sock.settimeout(self.upstream_timeout)
        return connection

    def forget(self, connection):
        with self._lock:
            self._active.discard(connection)
        connection.close()

    def redact(self, data):
        raw = self.key.encode()
        for token in (raw, raw[:8]):
            data = data.replace(token, b"[redacted]")
        return data

    def log(self, method, target, status, received, sent):
        # Method, path, status and byte counts only: no query, headers or bodies.
        path = "".join(char if 0x21 <= ord(char) <= 0x7e else "?" for char in str(target).split("?", 1)[0][:200])
        LOG.info("%s %s %s in=%d out=%d", method, path, status, received, sent)
