"""Real sockets on 127.0.0.1: a fake plain-HTTP upstream behind the real proxy."""
import http.client
import http.server
import json
import logging
import socket
import threading
import time
import unittest
from unittest.mock import patch

import key_proxy
from key_proxy import KeyProxy, Scrubber, holds_key

KEY = 'prx-fixture-upstream-key-not-exported'
CHAT = '/api/v1/chat/completions'
READ_CAP = 1024 * 1024
READ_DEADLINE = 10


def read_bounded(source, cap=READ_CAP, seconds=READ_DEADLINE):
    """Read a socket or an http.client response to EOF, with a byte cap and an absolute deadline.

    Returns (data, complete): `complete` is False when the peer cut the
    connection or broke the framing instead of ending the stream cleanly.
    """
    end = time.monotonic() + seconds
    data = bytearray()
    is_socket = isinstance(source, socket.socket)
    sock = source if is_socket else source.test_sock
    try:
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                raise AssertionError('read exceeded its absolute deadline')
            if len(data) > cap:
                raise AssertionError('read exceeded its byte cap')
            sock.settimeout(remaining)
            chunk = source.recv(65536) if is_socket else source.read1(65536)
            if not chunk:
                return bytes(data), True
            data.extend(chunk)
    except (ConnectionError, http.client.HTTPException):
        return bytes(data), False


class Upstream(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    timeout = 10

    def log_message(self, format, *args):
        del format, args

    def do_POST(self):
        length = int(self.headers.get('Content-Length', '0'))
        if length > READ_CAP:
            self.close_connection = True
            return
        body = self.rfile.read(length)
        self.server.seen.append({'path': self.path, 'headers': dict(self.headers.items()), 'body': body})
        mode = self.server.mode
        if mode in ('stream', 'split'):
            first, second = ((b'data: 1', b'data: 2') if mode == 'stream' else
                             (b'data: ' + KEY.encode()[:20], KEY.encode()[20:] + b' end'))
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Transfer-Encoding', 'chunked')
            self.end_headers()
            self.wfile.write(b'%x\r\n%b\r\n' % (len(first), first))
            self.wfile.flush()
            # Hold the reply open until the client has read the first part.
            self.server.first_read.wait(timeout=5)
            self.wfile.write(b'%x\r\n%b\r\n0\r\n\r\n' % (len(second), second))
            return
        if mode == 'close-stream':
            self.close_connection = True
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(b'data: 1')
            self.wfile.flush()
            self.server.first_read.wait(timeout=5)
            return
        if mode == 'bad-error':
            self.close_connection = True
            self.send_response(401)
            self.send_header('Transfer-Encoding', 'chunked')
            self.end_headers()
            self.wfile.write(b'zz\r\nnot a chunk')
            return
        if mode == 'short':
            self.close_connection = True
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', '100')
            self.end_headers()
            self.wfile.write(b'{"partial":')
            return
        if mode == 'denied':
            reply, status = json.dumps({'error': 'bad key ' + KEY}).encode(), 401
        else:
            reply, status = json.dumps({'ok': True}).encode(), 200
        self.send_response(status)
        self.send_header('Content-Type', 'application/json; echo=' + (KEY if mode == 'denied' else 'none'))
        self.send_header('Retry-After', KEY if mode == 'denied' else '1')
        self.send_header('Set-Cookie', 'upstream=1')
        self.send_header('Content-Length', str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)


class KeyProxyTests(unittest.TestCase):
    def setUp(self):
        self.upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        self.upstream.daemon_threads = True
        self.upstream.seen = []
        self.upstream.mode = 'json'
        self.upstream.first_read = threading.Event()
        threading.Thread(target=self.upstream.serve_forever, args=(0.05,), daemon=True).start()
        self.addCleanup(self.upstream.server_close)
        self.addCleanup(self.upstream.shutdown)
        self.addCleanup(self.upstream.first_read.set)
        self.proxy = self.make_proxy()

    def make_proxy(self, upstream=None, **options):
        port = self.upstream.server_address[1]
        proxy = KeyProxy(upstream or f'http://127.0.0.1:{port}/api/v1', KEY, '127.0.0.1',
                         **{'allow_http_upstream': True, 'max_body': 1024, **options})
        proxy.start()
        self.addCleanup(proxy.stop)
        return proxy

    def request(self, method, path, body=None, token=None, headers=None, proxy=None):
        connection = http.client.HTTPConnection(*(proxy or self.proxy).address, timeout=READ_DEADLINE)
        self.addCleanup(connection.close)
        sent = dict(headers or {})
        if token is not None:
            sent['Authorization'] = 'Bearer ' + token
        connection.request(method, path, body=body, headers=sent)
        sock = connection.sock  # http.client drops it from the connection on Connection: close
        response = connection.getresponse()
        response.test_sock = sock
        return response

    def body(self, response):
        data, complete = read_bounded(response)
        self.assertTrue(complete, 'response must end cleanly')
        return data

    def first(self, response, size):
        """One bounded read: at most `size` bytes, within the connection timeout."""
        return response.read1(size)

    def raw(self, payload=b'', proxy=None):
        client = socket.create_connection((proxy or self.proxy).address, timeout=5)
        self.addCleanup(client.close)
        if payload:
            client.sendall(payload)
        return client

    def head(self, extra=b'', target=CHAT.encode(), proxy=None):
        return (b'POST ' + target + b' HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer '
                + (proxy or self.proxy).token.encode() + b'\r\n' + extra + b'\r\n')

    @staticmethod
    def drain(client):
        return read_bounded(client)[0]

    def wait_for(self, condition, message):
        deadline = time.monotonic() + 5
        while not condition() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(condition(), message)

    def wait_idle(self, proxy=None):
        proxy = proxy or self.proxy
        self.wait_for(lambda: not proxy._exchanges, 'every exchange must be released')

    def assert_stops_clean(self, proxy, bound=2):
        handlers = [exchange.thread for exchange in list(proxy._exchanges.values())]
        started = time.monotonic()
        self.assertTrue(proxy.stop(), 'stop must leave no handler thread')
        self.assertLess(time.monotonic() - started, bound, 'stop must return within its bound')
        self.assertFalse([thread for thread in handlers if thread is not None and thread.is_alive()])
        self.assertFalse(proxy._thread.is_alive(), 'accept loop must be gone')
        self.assertEqual(proxy._exchanges, {}, 'no exchange, so no socket, may stay registered')

    def test_wrong_or_missing_token_is_401_without_upstream_call(self):
        for token in (None, 'wrong', KEY, self.proxy.token + 'x', ''):
            with self.subTest(token=token):
                response = self.request('POST', CHAT, b'{}', token)
                self.assertEqual(response.status, 401)
                self.assertEqual(self.body(response), b'')
        self.assertEqual(self.upstream.seen, [])

    def test_right_token_is_swapped_for_real_key(self):
        with self.assertLogs('prx_bench.key_proxy', logging.INFO) as logs:
            response = self.request('POST', CHAT, b'{"model":"m"}', self.proxy.token,
                                    {'Content-Type': 'application/json', 'X-Extra': 'dropped', 'Cookie': 'c=1'})
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(self.body(response)), {'ok': True})
            self.wait_idle()
        self.assertIsNone(response.getheader('Set-Cookie'), 'only allowlisted response headers pass')
        (seen,) = self.upstream.seen
        self.assertEqual(seen['headers']['Authorization'], 'Bearer ' + KEY)
        self.assertEqual(seen['body'], b'{"model":"m"}')
        self.assertEqual(seen['path'], CHAT)
        self.assertEqual(seen['headers']['Host'], '127.0.0.1:%d' % self.upstream.server_address[1])
        self.assertNotIn('X-Extra', seen['headers'])
        self.assertNotIn('Cookie', seen['headers'])
        self.assertNotIn(self.proxy.token, json.dumps(seen['headers']) + seen['path'])
        text = '\n'.join(logs.output)
        self.assertIn(f'POST {CHAT} 200 in=13 out=12', text)
        for hidden in (KEY, KEY[:8], self.proxy.token, 'model'):
            self.assertNotIn(hidden, text)

    def test_calls_outside_the_allowlist_are_403_without_upstream_call(self):
        calls = [('GET', CHAT), ('PUT', CHAT), ('DELETE', CHAT), ('POST', '/api/v1/models'),
                 ('GET', '/api/v1/models'), ('POST', '/api/v1/files'), ('POST', '/api/v1/responses'),
                 ('POST', CHAT + '?trace=query-fixture'), ('POST', CHAT + '/'), ('POST', CHAT + '/../files'),
                 ('POST', '/'), ('POST', '/api/v2/chat/completions'), ('POST', '/api/v1x/chat/completions'),
                 ('POST', '/api/v1/../v1/chat/completions'), ('POST', '/api/v1/%2e%2e/admin'),
                 ('POST', '/api/v1/%2Fchat/completions'), ('POST', '/api/v1//chat/completions'),
                 ('POST', '//evil.example' + CHAT), ('POST', '/other?next=' + CHAT),
                 ('POST', '/api/v1\\chat\\completions'), ('POST', CHAT.upper())]
        with self.assertLogs('prx_bench.key_proxy', logging.INFO) as logs:
            for method, target in calls:
                with self.subTest(method=method, target=target):
                    response = self.request(method, target, None if method in ('GET', 'DELETE') else b'{}',
                                            self.proxy.token)
                    self.assertEqual(response.status, 403)
                    self.assertEqual(self.body(response), b'')
            self.wait_idle()
        self.assertEqual(self.upstream.seen, [], 'a refused call must not reach the upstream')
        self.assertNotIn('query-fixture', '\n'.join(logs.output), 'queries are never logged')
        self.assertEqual(key_proxy.ALLOWED, (('POST', '/chat/completions'),))

    def test_raw_request_target_must_be_byte_identical(self):
        port = self.upstream.server_address[1]
        targets = [b'/' + CHAT.encode(),                       # http.server would fold the leading //
                   b'///' + CHAT.encode()[1:],
                   b'http://127.0.0.1:%d' % port + CHAT.encode(),  # absolute form naming the upstream itself
                   b'http://evil.example' + CHAT.encode(),
                   CHAT.encode() + b'#fragment']
        for target in targets:
            with self.subTest(target=target):
                reply = self.drain(self.raw(self.head(b'Content-Length: 2\r\n', target) + b'{}'))
                self.assertTrue(reply.startswith(b'HTTP/1.1 403 '), reply[:40])
        self.wait_idle()
        self.assertEqual(self.upstream.seen, [], 'only the literal target may reach the upstream')

    def test_streamed_response_passes_through_before_upstream_finishes(self):
        self.upstream.mode = 'stream'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader('Content-Type'), 'text/event-stream')
        # The upstream blocks after this event until first_read is set: a
        # buffering proxy would time out here instead of returning it.
        self.assertEqual(self.first(response, 64), b'data: 1')
        self.upstream.first_read.set()
        self.assertEqual(self.body(response), b'data: 2')

    def test_key_split_across_streamed_chunks_is_scrubbed(self):
        self.upstream.mode = 'split'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        # Only the possible key start is held back; the text before it arrives at once.
        self.assertEqual(self.first(response, 64), b'data: ')
        self.upstream.first_read.set()
        rest = self.body(response)
        self.assertEqual(rest, b'[redacted] end')
        self.assertFalse(holds_key(rest, KEY))

    def test_scrubber_equals_plain_replace_at_every_split(self):
        keys = (b'abcabcabc', b'aaaa', b'abab', b'sk-9fQ2mZx7Lr0TvB4nYc8HdK1pWs6EgJ3uAo5Ri')
        self.assertEqual(len(keys[-1]), 40)
        checked = 0
        for key in keys:
            bodies = [b'no secret here!',                      # zero occurrences
                      b'x ' + key + b' y!',                      # one
                      b'x ' + key + b' mid ' + key + b' y!',     # two
                      b'x' + key + key + b'y!',                  # adjacent
                      b'x' + key + key[1:] + b'y!',              # overlapping tails
                      b'x' + key[:-1] + key + b'y!',             # overlapping heads
                      key + key + key,                           # nothing but the key
                      b'x' + key[:len(key) // 2] + b'y' + key[len(key) // 2:] + b'!']  # never whole
            for body in bodies:
                expected = body.replace(key, b'[redacted]')
                size = len(body)
                for one in range(size + 1):
                    for two in range(one, size + 1):
                        scrubber = Scrubber(key)
                        out = scrubber.feed(body[:one])
                        self.assertLess(len(scrubber.held), len(key))
                        out += scrubber.feed(body[one:two]) + scrubber.feed(body[two:]) + scrubber.flush()
                        checked += 1
                        if out != expected or key in out:
                            self.fail(f'key {key!r} body {body!r} cuts {one},{two}: {out!r}')
                one_by_one = Scrubber(key)
                out = b''.join(one_by_one.feed(body[i:i + 1]) for i in range(size)) + one_by_one.flush()
                self.assertEqual(out, expected, f'byte-by-byte, key {key!r} body {body!r}')
        self.assertGreater(checked, 10000)

    def test_scrubber_replaces_a_trailing_partial_key(self):
        # Documented rule: a stream that ends inside a possible key occurrence
        # ends in the marker; no proper prefix of the key is emitted at EOF.
        for key in (b'abcabcabc', b'aaaa', b'abab', b'sk-9fQ2mZx7Lr0TvB4nYc8HdK1pWs6EgJ3uAo5Ri'):
            for size in range(1, len(key)):
                body = b'x' + key[:size]
                for one in range(len(body) + 1):
                    for two in range(one, len(body) + 1):
                        scrubber = Scrubber(key)
                        out = (scrubber.feed(body[:one]) + scrubber.feed(body[one:two])
                               + scrubber.feed(body[two:]) + scrubber.flush())
                        self.assertEqual(out, b'x[redacted]', f'key {key!r} prefix {size} cuts {one},{two}')
            whole = Scrubber(key)
            self.assertEqual(whole.feed(key), b'[redacted]', 'a complete key is replaced at once')
            self.assertEqual(whole.flush(), b'', 'nothing is left of a complete key')
        with self.assertRaises(ValueError):
            Scrubber(b'')

    def test_key_in_error_body_and_response_headers_is_scrubbed(self):
        self.upstream.mode = 'denied'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(response.status, 401)
        body = self.body(response)
        self.assertFalse(holds_key(body, KEY))
        self.assertEqual(json.loads(body), {'error': 'bad key [redacted]'})
        headers = '\n'.join(f'{name}: {value}' for name, value in response.getheaders())
        self.assertFalse(holds_key(headers.encode(), KEY), 'no header value may carry the key')
        self.assertEqual(response.getheader('Content-Type'), 'application/json; echo=[redacted]')
        self.assertEqual(response.getheader('Retry-After'), '[redacted]')

    def test_failed_upstream_error_body_yields_a_single_status_line(self):
        self.upstream.mode = 'bad-error'
        reply = self.drain(self.raw(self.head(b'Content-Length: 2\r\n') + b'{}'))
        self.assertTrue(reply.startswith(b'HTTP/1.1 401 '), reply[:40])
        self.assertEqual(reply.count(b'HTTP/1.1 '), 1, 'exactly one status line')
        self.assertFalse(reply.endswith(b'0\r\n\r\n'), 'a failed body must not be terminated as complete')

    def test_short_upstream_body_is_a_transport_failure_for_the_client(self):
        self.upstream.mode = 'short'
        reply = self.drain(self.raw(self.head(b'Content-Length: 2\r\n') + b'{}'))
        self.assertTrue(reply.startswith(b'HTTP/1.1 200 '), reply[:40])
        self.assertFalse(reply.endswith(b'0\r\n\r\n'), 'a truncated body must not be terminated as complete')
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        data, complete = read_bounded(response)
        self.assertFalse(complete, f'the client must see a broken stream, got {data!r}')

    def test_oversized_or_unbounded_body_is_refused_before_upstream(self):
        # Declared length only: the proxy must refuse before reading a body.
        reply = self.drain(self.raw(self.head(b'Content-Length: 1025\r\n')))
        self.assertTrue(reply.startswith(b'HTTP/1.1 413'), reply[:40])
        reply = self.drain(self.raw(self.head(b'Transfer-Encoding: chunked\r\n') + b'1\r\nx\r\n0\r\n\r\n'))
        self.assertTrue(reply.startswith(b'HTTP/1.1 411'), reply[:40])
        reply = self.drain(self.raw(self.head()))
        self.assertTrue(reply.startswith(b'HTTP/1.1 411'), reply[:40])
        self.assertEqual(self.upstream.seen, [])

    def test_late_body_after_stop_never_reaches_the_upstream(self):
        client = self.raw(self.head(b'Content-Length: 5\r\n'))
        self.wait_for(lambda: self.proxy._exchanges, 'the connection must be admitted')
        self.assert_stops_clean(self.proxy)
        try:
            client.sendall(b'hello')
            reply = self.drain(client)
        except OSError:
            reply = b''
        self.assertFalse(reply.startswith(b'HTTP/1.1 200'), reply[:40])
        self.assertEqual(self.upstream.seen, [], 'a body finished after stop must not be forwarded')
        late = key_proxy._Exchange(None, time.monotonic() + 60, time.monotonic() + 60)
        with self.assertRaises(OSError, msg='no upstream connection may be opened after stop'):
            self.proxy.connect(late)
        self.assertIsNone(late.upstream_sock)

    def test_expired_exchange_with_buffered_body_never_connects(self):
        for reaper in (True, False):
            with self.subTest(reaper=reaper):
                self.upstream.seen.clear()
                proxy = self.make_proxy(request_deadline=0.3)
                if not reaper:
                    # Without the reaper only the handler's own checks stand
                    # between a buffered body and the upstream.
                    proxy.reap = lambda: None
                real = proxy.authorized

                def slow(header, real=real):
                    time.sleep(0.6)  # the fixed pause is the scenario: the deadline passes mid-request
                    return real(header)

                with patch.object(proxy, 'authorized', side_effect=slow):
                    # Head and body arrive together, so the body is buffered
                    # in the handler before the deadline passes.
                    client = self.raw(self.head(b'Content-Length: 2\r\n', proxy=proxy) + b'{}', proxy=proxy)
                    reply = self.drain(client)
                    self.wait_idle(proxy)
                self.assertFalse(reply.startswith(b'HTTP/1.1 200'), reply[:40])
                self.assertEqual(self.upstream.seen, [], 'an expired exchange must make no upstream connection')
        past = time.monotonic() - 1
        expired = key_proxy._Exchange(None, past, past)
        with self.assertRaises(OSError):
            self.proxy.check(expired)
        self.assertTrue(expired.cancelled, 'expiry is terminal')
        expired.deadline = expired.read_by = time.monotonic() + 60
        with self.assertRaises(OSError, msg='a cancelled exchange stays cancelled'):
            self.proxy.connect(expired)
        self.assertEqual(self.upstream.seen, [])

    def black_hole(self):
        """A listener that completes no exchange: it never accepts and its backlog is full."""
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.addCleanup(listener.close)
        listener.bind(('127.0.0.1', 0))
        listener.listen(0)
        for _ in range(4):
            filler = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.addCleanup(filler.close)
            filler.setblocking(False)
            filler.connect_ex(listener.getsockname())
        return listener.getsockname()[1]

    def test_stop_during_connect_or_handshake_to_a_black_hole_leaves_nothing(self):
        for scheme in ('http', 'https'):
            with self.subTest(scheme=scheme):
                port = self.black_hole()
                options = {} if scheme == 'http' else {'allow_http_upstream': False}
                proxy = self.make_proxy(f'{scheme}://127.0.0.1:{port}/api/v1', **options)
                self.raw(self.head(b'Content-Length: 2\r\n', proxy=proxy) + b'{}', proxy=proxy)
                # The upstream socket is registered before the connect starts,
                # so from here the handler is inside connect, handshake or the
                # wait for a response that never comes.
                self.wait_for(lambda: any(exchange.upstream_sock is not None
                                          for exchange in list(proxy._exchanges.values())),
                              'the handler must have started its upstream connection')
                self.assert_stops_clean(proxy)

    def test_stop_cuts_a_streaming_connection_close_response(self):
        self.upstream.mode = 'close-stream'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(self.first(response, 64), b'data: 1')
        self.assert_stops_clean(self.proxy)
        data, complete = read_bounded(response)
        self.assertFalse(complete, f'the stream must be cut, got {data!r}')

    def test_stop_kills_the_token_and_the_listener(self):
        address = self.proxy.address
        self.assertTrue(self.proxy.stop())
        self.assertTrue(self.proxy.stop(), 'stop is idempotent')
        with self.assertRaises(OSError):
            socket.create_connection(address, timeout=2).close()

    def test_connection_flood_is_capped_before_any_read(self):
        proxy = self.make_proxy(max_inflight=2)
        idle = [self.raw(proxy=proxy), self.raw(proxy=proxy)]
        self.wait_for(lambda: len(proxy._exchanges) == 2, 'both idle connections must be admitted')
        for _ in range(8):
            extra = self.raw(proxy=proxy)
            # Dropped at accept: closed without a response and without a handler thread.
            self.assertEqual(self.drain(extra), b'')
            self.assertEqual(len(proxy._exchanges), 2)
        for client in idle:
            client.close()
        self.wait_idle(proxy)
        response = self.request('POST', CHAT, b'{}', proxy.token, proxy=proxy)
        self.assertEqual(response.status, 200, 'capacity returns when connections end')
        self.body(response)

    def test_absolute_deadline_cuts_a_slow_client(self):
        proxy = self.make_proxy(read_deadline=0.3)
        client = self.raw(b'POST ' + CHAT.encode() + b' HTTP/1.1\r\n', proxy=proxy)
        started = time.monotonic()
        # The 60 second inactivity timeout alone would keep this open.
        self.assertEqual(self.drain(client), b'')
        self.assertLess(time.monotonic() - started, 4)
        self.wait_idle(proxy)

    def test_peer_filter_drops_other_sources_unread(self):
        proxy = self.make_proxy(allowed_peers=['10.255.255.0/24'])
        self.assertEqual(self.drain(self.raw(proxy=proxy)), b'')
        self.assertEqual(proxy._exchanges, {}, 'a filtered peer takes no slot')
        allowed = self.make_proxy(allowed_peers=['127.0.0.0/8'])
        response = self.request('POST', CHAT, b'{}', allowed.token, proxy=allowed)
        self.assertEqual(response.status, 200)
        self.body(response)

    def test_unreachable_upstream_is_a_bare_502_and_leaves_nothing_registered(self):
        self.upstream.shutdown()
        self.upstream.server_close()
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(response.status, 502)
        self.assertEqual(self.body(response), b'')
        self.wait_idle()

    def test_constructor_refuses_unsafe_upstreams_and_keys(self):
        for upstream in ('http://127.0.0.1:1/v1', 'https://u:p@host.example/v1', 'https://127.0.0.1',
                         'https://127.0.0.1/', 'https://127.0.0.1/v1?x=1', 'ftp://127.0.0.1/v1',
                         'https://127.0.0.1/v1/../x', 'https://127.0.0.1/v1/%2e%2e', 'https://127.0.0.1//v1'):
            with self.subTest(upstream=upstream), self.assertRaises(ValueError):
                KeyProxy(upstream, KEY, '127.0.0.1')
        with self.assertRaises(ValueError) as caught:
            KeyProxy('https://127.0.0.1/v1', 'line\r\nbreak-fixture', '127.0.0.1')
        self.assertNotIn('break-fixture', str(caught.exception))

    def test_holds_key_matches_full_key_and_eight_byte_prefix(self):
        self.assertTrue(holds_key(b'api_key = "' + KEY.encode() + b'"', KEY))
        self.assertTrue(holds_key(b'x ' + KEY.encode()[:8] + b' y', KEY))
        self.assertFalse(holds_key(b'api_key = "per-trial-token"', KEY))


if __name__ == '__main__':
    unittest.main()
