"""Host-side forwarding proxy that keeps the paid provider key out of the task container.

The adapter starts one proxy per trial. The container's PRX profile points at
the proxy and holds only a per-trial random bearer token; the proxy swaps that
token for the real key and forwards the allowlisted inference calls to one
fixed HTTPS upstream. Standard library only. Nothing here logs or returns
headers, bodies or credentials, and the real key is scrubbed from everything
sent back.

Operator notes (also printed by `run_tb2.sh --help`):
- The listener is reachable by anything that can reach its bind address (by
  default the host's primary IPv4 address, so the LAN). The adapter cannot
  learn the container network's addresses (Harbor's compose file creates the
  network), so by default the token is the only gate. A client without the
  token gets nothing forwarded, but it can hold the 16 connection slots for
  up to 120 seconds each and so delay or fail a trial.
- Run the benchmark host on an isolated network, or pass `--proxy-peers` with
  the source network container traffic arrives from; other peers are then
  dropped at accept, before a slot is taken.
- The upstream host is resolved once, off the event loop and under a startup
  deadline (RESOLVE_DEADLINE), before the proxy exists; a trial connects only
  to those addresses until it ends.
"""
import asyncio
import errno
import hmac
import http.client
import http.server
import ipaddress
import logging
import secrets
import select
import socket
import ssl
import threading
import time
from urllib.parse import urlsplit

MAX_BODY = 32 * 1024 * 1024
MAX_INFLIGHT = 16
MAX_ADDRESSES = 8
CLIENT_TIMEOUT = 60      # inactivity, client socket
CONNECT_TIMEOUT = 10     # upstream TCP connect plus TLS handshake, per address
UPSTREAM_TIMEOUT = 600   # inactivity, upstream socket
READ_DEADLINE = 120      # absolute: request head and body fully received
REQUEST_DEADLINE = 1800  # absolute: whole exchange, upstream response included
STOP_JOIN = 5            # stop(): every blocking call is cut or polls within POLL
RESOLVE_DEADLINE = 5     # absolute: upstream DNS resolution at startup
POLL = 0.05
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


def resolve(host, port):
    """Resolve the upstream; blocking, with no bound: only ever call it off the event loop."""
    addresses = []
    for family, _, _, _, address in socket.getaddrinfo(host, port, type=socket.SOCK_STREAM):
        if (family, address) not in addresses and len(addresses) < MAX_ADDRESSES:
            addresses.append((family, address))
    return addresses


async def resolve_bounded(host, port, resolver=None, deadline=RESOLVE_DEADLINE):
    """Resolve without blocking the event loop, within an absolute `deadline`.

    The standard library cannot cancel a DNS lookup, so the lookup runs in a
    daemon thread that is abandoned, never joined, on timeout or cancellation:
    the awaiting coroutine returns at once and a stuck resolver cannot hold up
    the trial or interpreter exit. Its late result is dropped. Errors are
    reduced to a type name; they never carry key material.
    """
    resolver = resolver or resolve
    loop = asyncio.get_running_loop()
    future = loop.create_future()

    def deliver(outcome, value):
        if not future.done():
            (future.set_result if outcome else future.set_exception)(value)

    def work():
        try:
            outcome, value = True, resolver(host, port)
        except Exception as error:  # reported to the awaiting coroutine
            outcome, value = False, error
        try:
            loop.call_soon_threadsafe(deliver, outcome, value)
        except RuntimeError:  # the loop is already closed: nobody is waiting
            pass

    threading.Thread(target=work, name="key-proxy-resolve", daemon=True).start()
    try:
        addresses = await asyncio.wait_for(future, deadline)
    except asyncio.TimeoutError:
        raise OSError(f"upstream DNS resolution did not finish within {deadline:g} seconds") from None
    except OSError as error:
        raise OSError(f"cannot resolve the upstream host ({type(error).__name__})") from None
    if not addresses:
        raise OSError("upstream host has no address")
    return addresses


def upstream_endpoint(upstream, allow_http_upstream=False):
    """(host, port) of a validated upstream base URL; ValueError otherwise."""
    parts = urlsplit(upstream)
    if parts.scheme not in (("https", "http") if allow_http_upstream else ("https",)):
        raise ValueError("proxy upstream must be an https URL")
    if not parts.hostname:
        raise ValueError("proxy upstream must be a plain base URL")
    return parts.hostname, parts.port or (443 if parts.scheme == "https" else 80)


async def open_proxy(upstream, key, bind, *, resolver=None, resolve_deadline=RESOLVE_DEADLINE, **options):
    """Resolve the upstream off the event loop, then build the (not yet started) KeyProxy."""
    host, port = upstream_endpoint(upstream, options.get("allow_http_upstream", False))
    addresses = await resolve_bounded(host, port, resolver, resolve_deadline)
    return KeyProxy(upstream, key, bind, addresses=addresses, **options)


def holds_key(data, key):
    """True when data contains the key or its 8-byte prefix."""
    raw = key.encode()
    return any(token and token in data for token in (raw, raw[:8]))


def _printable(value):
    return all(0x21 <= ord(char) <= 0x7e for char in value)


class _Cancelled(OSError):
    """The exchange was cancelled: proxy stopped, deadline passed, or cut."""


class Scrubber:
    """Streaming `data.replace(key, MARK)` that never lets the key out in pieces.

    feed() replaces every complete occurrence (leftmost first, exactly as
    bytes.replace does on the whole stream) and emits every byte that can no
    longer be part of an occurrence. The only bytes held back are the longest
    tail of the stream that is a proper prefix of the key, at most key length
    minus one, because an occurrence can only continue from such a tail.

    flush() ends the stream. Whatever is still held is a proper prefix of the
    key (the stream stopped where the key could have been in progress), and it
    is replaced by the marker rather than emitted. The cost of that rule: a
    stream whose last bytes happen to equal the first bytes of the key ends in
    the marker instead.
    """

    def __init__(self, key):
        if not key:
            raise ValueError("empty key")
        self.key = key
        self.held = b""

    def feed(self, data):
        buffer = self.held + data
        key = self.key
        out = bytearray()
        start = 0
        while True:
            found = buffer.find(key, start)
            if found < 0:
                break
            out += buffer[start:found]
            out += MARK
            start = found + len(key)
        rest = buffer[start:]
        keep = 0
        limit = min(len(rest), len(key) - 1)
        if limit and key[:1] in rest[-limit:]:
            for size in range(limit, 0, -1):
                if rest.endswith(key[:size]):
                    keep = size
                    break
        out += rest[:len(rest) - keep]
        self.held = rest[len(rest) - keep:]
        return bytes(out)

    def flush(self):
        out, self.held = (MARK if self.held else b""), b""
        return out


class _Exchange:
    """One accepted client connection and, later, its upstream socket.

    Both deadlines are absolute, set once at admission and never replaced.
    Cancellation is terminal.
    """

    def __init__(self, client, read_by, deadline):
        self.client = client
        self.read_by = read_by      # request head and body fully received
        self.deadline = deadline    # whole exchange
        self.body_done = False
        self.cancelled = False
        self.upstream_sock = None   # registered before connect(); TLS replaces it before the handshake
        self.thread = None

    def expired(self, now):
        return now >= self.deadline or (not self.body_done and now >= self.read_by)


def _shut(sock):
    if sock is not None:
        try:
            # The plain method: SSLSocket.shutdown would first try a TLS unwrap.
            socket.socket.shutdown(sock, socket.SHUT_RDWR)
        except (OSError, ValueError):
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
                raise _Cancelled()
            self._relay(proxy, self.exchange)
        except (OSError, http.client.HTTPException, ValueError) as error:
            # Exception text may quote header values; report a bare status only.
            # After the head went out the only honest signal is a cut connection.
            if not self.head_sent:
                try:
                    self._head(503 if isinstance(error, _Cancelled) else 502)
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

    def _raw_target(self):
        """The request target exactly as sent; http.server rewrites `self.path`."""
        words = self.raw_requestline.split(b" ")
        return words[1] if len(words) == 3 else None

    def _relay(self, proxy, exchange):
        proxy.check(exchange)
        if not proxy.authorized(self.headers.get("Authorization")):
            return self._head(401)
        path = proxy.allowed_path(self.command, self._raw_target())
        if path is None:
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
        proxy.check(exchange)
        body = self.rfile.read(length) if length else b""
        self.received = len(body)
        if len(body) != length:
            return self._head(400)
        # A body that was already buffered when the exchange expired or was
        # cut still arrives here; it must not start an upstream request.
        proxy.check(exchange)
        exchange.body_done = True
        connection = proxy.connect(exchange)
        try:
            connection.putrequest(self.command, path, skip_accept_encoding=True)
            for name in REQUEST_HEADERS:
                value = self.headers.get(name)
                if value is not None:
                    connection.putheader(name, value)
            connection.putheader("Accept-Encoding", "identity")
            connection.putheader("Authorization", "Bearer " + proxy.key)
            connection.putheader("Content-Length", str(length))
            connection.endheaders(body or None)
            response = connection.getresponse()
            try:
                self._respond(proxy, exchange, response)
            finally:
                response.close()
        finally:
            connection.close()

    def _respond(self, proxy, exchange, response):
        encoding = response.getheader("Content-Encoding")
        if encoding is not None and encoding.strip().lower() != "identity":
            raise ValueError("encoded upstream body cannot be scrubbed")
        headers = []
        for name in RESPONSE_HEADERS:
            value = response.getheader(name)
            if value is None:
                continue
            value = proxy.scrub_value(value)
            if "\r" not in value and "\n" not in value:
                headers.append((name.title(), value))
        proxy.check(exchange)
        if response.status in (204, 304):
            return self._head(response.status, headers, "Upstream")
        headers.append(("Transfer-Encoding", "chunked"))
        self._head(response.status, headers, "Upstream")
        # Stream as bytes arrive so SSE and chunked replies are never buffered
        # whole; success and error bodies alike pass through the scrubber.
        scrubber = Scrubber(proxy.key.encode())
        while True:
            data = response.read1(CHUNK)
            proxy.check(exchange)
            if not data and response.length:
                # Upstream closed before its Content-Length was delivered: cut
                # the client too, so it sees a transport failure, not a reply.
                raise ValueError("short upstream body")
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

    def __init__(self, upstream, key, bind, port=0, *, addresses, allow_http_upstream=False, allowed_peers=None,
                 upstream_timeout=UPSTREAM_TIMEOUT, max_body=MAX_BODY, max_inflight=MAX_INFLIGHT,
                 read_deadline=READ_DEADLINE, request_deadline=REQUEST_DEADLINE):
        parts = urlsplit(upstream)
        schemes = ("https", "http") if allow_http_upstream else ("https",)
        if parts.scheme not in schemes:
            raise ValueError("proxy upstream must be an https URL")
        if not parts.hostname or parts.username or parts.password or parts.query or parts.fragment:
            raise ValueError("proxy upstream must be a plain base URL")
        prefix = parts.path.rstrip("/")
        if (not prefix.startswith("/") or prefix.startswith("//") or not _printable(prefix) or "\\" in prefix
                or "%" in prefix or any(segment in (".", "..") for segment in prefix.split("/"))):
            raise ValueError("proxy upstream needs a plain, non-empty API path prefix")
        if not key or not _printable(key):
            # Never echo the value: http.client would quote it in its own error.
            raise ValueError("key file content is not a usable bearer credential")
        self.peers = None
        if allowed_peers is not None:
            self.peers = [ipaddress.ip_network(peer.strip(), strict=False) for peer in allowed_peers]
        self.scheme, self.host, self.prefix = parts.scheme, parts.hostname, prefix
        self.port = parts.port or (443 if parts.scheme == "https" else 80)
        # Resolved by the caller (open_proxy) before construction, so neither
        # the event loop nor a handler ever waits in a DNS lookup.
        if not addresses:
            raise ValueError("proxy upstream needs at least one resolved address")
        self.addresses = list(addresses)[:MAX_ADDRESSES]
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
        self._thread = threading.Thread(target=self._server.serve_forever, args=(POLL,), daemon=True)
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
        now = time.monotonic()
        with self._lock:
            if self.stopped.is_set() or len(self._exchanges) >= self.max_inflight:
                return None
            exchange = _Exchange(request, now + min(self.read_deadline, self.request_deadline),
                                 now + self.request_deadline)
            self._exchanges[id(request)] = exchange
            return exchange

    def exchange_for(self, request):
        with self._lock:
            return self._exchanges.get(id(request))

    def cancel(self, exchange):
        """Terminal: flag the exchange and shut both of its sockets."""
        with self._lock:
            exchange.cancelled = True
            sockets = (exchange.client, exchange.upstream_sock)
        for sock in sockets:
            _shut(sock)

    def check(self, exchange):
        """Raise unless the exchange may still read, connect or forward."""
        if exchange.cancelled or self.stopped.is_set():
            raise _Cancelled()
        if exchange.expired(time.monotonic()):
            self.cancel(exchange)
            raise _Cancelled()

    def _register(self, exchange, sock):
        """Make `sock` the exchange's tracked upstream socket, or refuse it."""
        with self._lock:
            if not (exchange.cancelled or self.stopped.is_set() or exchange.expired(time.monotonic())):
                exchange.upstream_sock = sock
                return
        raise _Cancelled()

    def release(self, exchange):
        with self._lock:
            self._exchanges.pop(id(exchange.client), None)
            sock, exchange.upstream_sock = exchange.upstream_sock, None
        if sock is not None:
            try:
                sock.close()
            except OSError:
                pass

    def reap(self):
        """Cancel every exchange past a deadline; called from the accept loop."""
        now = time.monotonic()
        with self._lock:
            late = [exchange for exchange in self._exchanges.values()
                    if not exchange.cancelled and exchange.expired(now)]
        for exchange in late:
            self.cancel(exchange)

    def stop(self, join_timeout=STOP_JOIN):
        """Stop serving, cancel every exchange and wait for its handler.

        Returns True when no handler thread is left; the caller must treat
        False as a failed trial. After the flag is set no handler can register
        an upstream socket; every registered socket, including one inside a
        TCP connect or TLS handshake, is shut down here, and those two steps
        also poll the flag every POLL seconds, so each handler ends well
        within `join_timeout`. The token is dead afterwards.
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
            self.cancel(exchange)
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

    def allowed_path(self, method, raw_target):
        """The upstream path for an allowlisted call, else None.

        `raw_target` is the request target as bytes from the request line. It
        must be byte-identical to `<prefix><path>`: no query, no extra slash,
        no absolute form. The upstream host is never taken from the request.
        """
        for allowed, suffix in ALLOWED:
            path = self.prefix + suffix
            if method == allowed and raw_target == path.encode():
                return path
        return None

    def _until(self, exchange, sock, end, step):
        """Run one non-blocking step to completion, polling for cancellation."""
        while True:
            self.check(exchange)
            if time.monotonic() >= end:
                raise TimeoutError("upstream connect timed out")
            wait = step()
            if wait is None:
                return
            select.select(*(([sock], [], []) if wait == "read" else ([], [sock], [])), POLL)

    def connect(self, exchange):
        """Open the upstream connection for one exchange.

        The socket is registered with the exchange before the TCP connect
        starts and again when TLS takes it over, and both steps are
        non-blocking, so stop() and expiry abort them at once.
        """
        failure = OSError("upstream has no address")
        for family, address in self.addresses:
            self.check(exchange)
            end = time.monotonic() + CONNECT_TIMEOUT
            sock = socket.socket(family, socket.SOCK_STREAM)
            try:
                sock.setblocking(False)
                self._register(exchange, sock)
                code = sock.connect_ex(address)
                if code not in (0, errno.EINPROGRESS, errno.EWOULDBLOCK, errno.EALREADY):
                    raise OSError(code, "upstream connect failed")

                def connected(sock=sock):
                    if not select.select([], [sock], [], 0)[1]:
                        return "write"
                    code = sock.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
                    if code:
                        raise OSError(code, "upstream connect failed")
                    return None

                self._until(exchange, sock, end, connected)
                if self.tls is not None:
                    sock = self.tls.wrap_socket(sock, server_hostname=self.host, do_handshake_on_connect=False)
                    self._register(exchange, sock)

                    def shaken(sock=sock):
                        try:
                            sock.do_handshake()
                        except ssl.SSLWantReadError:
                            return "read"
                        except ssl.SSLWantWriteError:
                            return "write"
                        return None

                    self._until(exchange, sock, end, shaken)
                sock.settimeout(self.upstream_timeout)
                self.check(exchange)
            except _Cancelled:
                sock.close()
                raise
            except (OSError, ValueError) as error:
                with self._lock:
                    exchange.upstream_sock = None
                sock.close()
                failure = error
                continue
            kind = http.client.HTTPSConnection if self.tls is not None else http.client.HTTPConnection
            options = {"context": self.tls} if self.tls is not None else {}
            connection = kind(self.host, self.port, **options)
            connection.sock = sock  # already connected: http.client never opens its own
            return connection
        raise failure

    def scrub_value(self, value):
        """A header value is a complete string: plain replacement."""
        return value.replace(self.key, MARK.decode())

    def log(self, method, target, status, received, sent):
        # Method, path, status and byte counts only: no query, headers or bodies.
        path = "".join(char if 0x21 <= ord(char) <= 0x7e else "?" for char in str(target).split("?", 1)[0][:200])
        LOG.info("%s %s %s in=%d out=%d", method, path, status, received, sent)
