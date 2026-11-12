"""Real sockets on 127.0.0.1: a fake plain-HTTP upstream behind the real proxy."""
import http.client
import http.server
import json
import logging
import socket
import threading
import time
import unittest

import key_proxy
from key_proxy import KeyProxy, Scrubber, holds_key

KEY = 'prx-fixture-upstream-key-not-exported'
CHAT = '/api/v1/chat/completions'


class Upstream(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, format, *args):
        del format, args

    def do_GET(self):
        self.do_POST()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        self.server.seen.append({'path': self.path, 'headers': dict(self.headers.items()), 'body': body})
        mode = self.server.mode
        if mode in ('stream', 'split'):
            first, second = ((b'data: 1', b'data: 2') if mode == 'stream' else
                             (b'data: ' + KEY.encode()[:20], KEY.encode()[20:] + b' ' + KEY.encode()[:12] + b'!'))
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
        if mode == 'denied':
            reply, status = json.dumps({'error': 'bad key ' + KEY}).encode(), 401
        else:
            reply, status = json.dumps({'ok': True}).encode(), 200
        self.send_response(status)
        self.send_header('Content-Type', 'application/json; echo=' + (KEY if mode == 'denied' else 'none'))
        self.send_header('Retry-After', KEY[:10] if mode == 'denied' else '1')
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

    def make_proxy(self, **options):
        port = self.upstream.server_address[1]
        proxy = KeyProxy(f'http://127.0.0.1:{port}/api/v1', KEY, '127.0.0.1',
                         allow_http_upstream=True, **{'max_body': 1024, **options})
        proxy.start()
        self.addCleanup(proxy.stop)
        return proxy

    def request(self, method, path, body=None, token=None, headers=None, proxy=None):
        connection = http.client.HTTPConnection(*(proxy or self.proxy).address, timeout=10)
        self.addCleanup(connection.close)
        sent = dict(headers or {})
        if token is not None:
            sent['Authorization'] = 'Bearer ' + token
        connection.request(method, path, body=body, headers=sent)
        return connection.getresponse()

    def raw(self, payload=b'', proxy=None):
        client = socket.create_connection((proxy or self.proxy).address, timeout=5)
        self.addCleanup(client.close)
        if payload:
            client.sendall(payload)
        return client

    def head(self, extra=b''):
        return (b'POST ' + CHAT.encode() + b' HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer '
                + self.proxy.token.encode() + b'\r\n' + extra + b'\r\n')

    @staticmethod
    def drain(client):
        data = bytearray()
        try:
            while chunk := client.recv(65536):
                data.extend(chunk)
        except ConnectionError:
            pass
        return bytes(data)

    def wait_idle(self, proxy=None):
        proxy = proxy or self.proxy
        deadline = time.monotonic() + 5
        while proxy._exchanges and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertEqual(proxy._exchanges, {}, 'every exchange must be released')

    def test_wrong_or_missing_token_is_401_without_upstream_call(self):
        for token in (None, 'wrong', KEY, self.proxy.token + 'x', ''):
            with self.subTest(token=token):
                response = self.request('POST', CHAT, b'{}', token)
                self.assertEqual(response.status, 401)
                self.assertEqual(response.read(), b'')
        self.assertEqual(self.upstream.seen, [])

    def test_right_token_is_swapped_for_real_key(self):
        with self.assertLogs('prx_bench.key_proxy', logging.INFO) as logs:
            response = self.request('POST', CHAT, b'{"model":"m"}', self.proxy.token,
                                    {'Content-Type': 'application/json', 'X-Extra': 'dropped', 'Cookie': 'c=1'})
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(response.read()), {'ok': True})
            self.wait_idle()
        self.assertIsNone(response.getheader('Set-Cookie'), 'only allowlisted response headers pass')
        (seen,) = self.upstream.seen
        self.assertEqual(seen['headers']['Authorization'], 'Bearer ' + KEY)
        self.assertEqual(seen['body'], b'{"model":"m"}')
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
                 ('POST', '/api/v1/%2Fchat/completions'), ('POST', '//evil.example' + CHAT),
                 ('POST', 'http://evil.example' + CHAT), ('POST', '/other?next=' + CHAT),
                 ('POST', '/api/v1\\chat\\completions')]
        with self.assertLogs('prx_bench.key_proxy', logging.INFO) as logs:
            for method, target in calls:
                with self.subTest(method=method, target=target):
                    response = self.request(method, target, None if method in ('GET', 'DELETE') else b'{}',
                                            self.proxy.token)
                    self.assertEqual(response.status, 403)
                    self.assertEqual(response.read(), b'')
            self.wait_idle()
        self.assertEqual(self.upstream.seen, [], 'a refused call must not reach the upstream')
        self.assertNotIn('query-fixture', '\n'.join(logs.output), 'queries are never logged')
        self.assertEqual(key_proxy.ALLOWED, (('POST', '/chat/completions'),))

    def test_streamed_response_passes_through_before_upstream_finishes(self):
        self.upstream.mode = 'stream'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader('Content-Type'), 'text/event-stream')
        # The upstream blocks after this event until first_read is set: a
        # buffering proxy would time out here instead of returning it.
        self.assertEqual(response.read1(64), b'data: 1')
        self.upstream.first_read.set()
        self.assertEqual(response.read(), b'data: 2')

    def test_key_split_across_streamed_chunks_is_scrubbed(self):
        self.upstream.mode = 'split'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        # Only the possible key start is held back; the text before it arrives at once.
        self.assertEqual(response.read1(64), b'data: ')
        self.upstream.first_read.set()
        rest = response.read()
        self.assertEqual(rest, b'[redacted] [redacted]!')
        self.assertFalse(holds_key(rest, KEY))

    def test_scrubber_catches_the_key_at_every_split_point(self):
        raw = KEY.encode()
        text = b'head ' + raw + b' mid ' + raw[:9] + b' ' + raw[:7] + b' tail ' + raw
        expected = b'head [redacted] mid [redacted] ' + raw[:7] + b' tail [redacted]'
        for cut in range(len(text) + 1):
            scrubber = Scrubber(raw)
            first = scrubber.feed(text[:cut])
            self.assertLess(len(scrubber.held), len(raw), 'at most key length minus one is held')
            self.assertEqual(first + scrubber.feed(text[cut:]) + scrubber.flush(), expected, f'split at {cut}')
        one_by_one = Scrubber(raw)
        out = b''.join(one_by_one.feed(text[i:i + 1]) for i in range(len(text))) + one_by_one.flush()
        self.assertEqual(out, expected)

    def test_key_in_error_body_and_response_headers_is_scrubbed(self):
        self.upstream.mode = 'denied'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(response.status, 401)
        body = response.read()
        self.assertFalse(holds_key(body, KEY))
        self.assertIn(b'[redacted]', body)
        headers = '\n'.join(f'{name}: {value}' for name, value in response.getheaders())
        self.assertFalse(holds_key(headers.encode(), KEY), 'no header value may carry the key')
        self.assertEqual(response.getheader('Content-Type'), 'application/json; echo=[redacted]')
        self.assertEqual(response.getheader('Retry-After'), '[redacted]')

    def test_failed_upstream_error_body_yields_a_single_status_line(self):
        self.upstream.mode = 'bad-error'
        client = self.raw(self.head(b'Content-Length: 2\r\n') + b'{}')
        reply = self.drain(client)
        self.assertTrue(reply.startswith(b'HTTP/1.1 401 '), reply[:40])
        self.assertEqual(reply.count(b'HTTP/1.1 '), 1, 'exactly one status line')
        self.assertFalse(reply.endswith(b'0\r\n\r\n'), 'a failed body must not be terminated as complete')

    def test_oversized_or_unbounded_body_is_refused_before_upstream(self):
        # Declared length only: the proxy must refuse before reading a body.
        client = self.raw(self.head(b'Content-Length: 1025\r\n'))
        self.assertTrue(client.recv(64).startswith(b'HTTP/1.1 413'))
        client = self.raw(self.head(b'Transfer-Encoding: chunked\r\n') + b'1\r\nx\r\n0\r\n\r\n')
        self.assertTrue(client.recv(64).startswith(b'HTTP/1.1 411'))
        client = self.raw(self.head())
        self.assertTrue(client.recv(64).startswith(b'HTTP/1.1 411'))
        self.assertEqual(self.upstream.seen, [])

    def test_late_body_after_stop_never_reaches_the_upstream(self):
        client = self.raw(self.head(b'Content-Length: 5\r\n'))
        deadline = time.monotonic() + 5
        while not self.proxy._exchanges and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(self.proxy.stop(), 'stop must leave no handler thread')
        try:
            client.sendall(b'hello')
            reply = self.drain(client)
        except OSError:
            reply = b''
        self.assertFalse(reply.startswith(b'HTTP/1.1 200'), reply[:40])
        self.assertEqual(self.upstream.seen, [], 'a body finished after stop must not be forwarded')
        with self.assertRaises(OSError, msg='no upstream connection may be opened after stop'):
            self.proxy.connect(key_proxy._Exchange(None, 0))
        self.assertEqual(self.proxy._exchanges, {})

    def test_stop_cuts_a_streaming_connection_close_response(self):
        self.upstream.mode = 'close-stream'
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(response.read1(64), b'data: 1')
        started = time.monotonic()
        self.assertTrue(self.proxy.stop(), 'stop must end the handler although the upstream is still open')
        self.assertLess(time.monotonic() - started, 4, 'stop must not wait for the upstream')
        with self.assertRaises((http.client.HTTPException, OSError)):
            response.read()
        self.assertEqual(self.proxy._exchanges, {})

    def test_stop_kills_the_token_and_the_listener(self):
        address = self.proxy.address
        self.assertTrue(self.proxy.stop())
        self.assertTrue(self.proxy.stop(), 'stop is idempotent')
        with self.assertRaises(OSError):
            socket.create_connection(address, timeout=2).close()

    def test_connection_flood_is_capped_before_any_read(self):
        proxy = self.make_proxy(max_inflight=2)
        idle = [self.raw(proxy=proxy), self.raw(proxy=proxy)]
        deadline = time.monotonic() + 5
        while len(proxy._exchanges) < 2 and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertEqual(len(proxy._exchanges), 2)
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

    def test_absolute_deadline_cuts_a_slow_client(self):
        proxy = self.make_proxy(read_deadline=0.3)
        client = self.raw(b'POST ' + CHAT.encode() + b' HTTP/1.1\r\n', proxy=proxy)
        started = time.monotonic()
        # The 60 second inactivity timeout alone would keep this open; the 5
        # second client timeout would then fail the read below.
        self.assertEqual(self.drain(client), b'')
        self.assertLess(time.monotonic() - started, 4)
        self.wait_idle(proxy)

    def test_peer_filter_drops_other_sources_unread(self):
        proxy = self.make_proxy(allowed_peers=['10.255.255.0/24'])
        self.assertEqual(self.drain(self.raw(self.head(b'Content-Length: 2\r\n') + b'{}', proxy=proxy)), b'')
        self.assertEqual(self.upstream.seen, [])
        allowed = self.make_proxy(allowed_peers=['127.0.0.0/8'])
        self.assertEqual(self.request('POST', CHAT, b'{}', allowed.token, proxy=allowed).status, 200)

    def test_unreachable_upstream_is_a_bare_502_and_leaves_nothing_registered(self):
        self.upstream.shutdown()
        self.upstream.server_close()
        response = self.request('POST', CHAT, b'{}', self.proxy.token)
        self.assertEqual(response.status, 502)
        self.assertEqual(response.read(), b'')
        self.wait_idle()

    def test_constructor_refuses_unsafe_upstreams_and_keys(self):
        for upstream in ('http://127.0.0.1:1/v1', 'https://u:p@host.example/v1', 'https://host.example',
                         'https://host.example/', 'https://host.example/v1?x=1', 'ftp://host.example/v1',
                         'https://host.example/v1/../x', 'https://host.example/v1/%2e%2e'):
            with self.subTest(upstream=upstream), self.assertRaises(ValueError):
                KeyProxy(upstream, KEY, '127.0.0.1')
        with self.assertRaises(ValueError) as caught:
            KeyProxy('https://host.example/v1', 'line\r\nbreak-fixture', '127.0.0.1')
        self.assertNotIn('break-fixture', str(caught.exception))

    def test_holds_key_matches_full_key_and_eight_byte_prefix(self):
        self.assertTrue(holds_key(b'api_key = "' + KEY.encode() + b'"', KEY))
        self.assertTrue(holds_key(b'x ' + KEY.encode()[:8] + b' y', KEY))
        self.assertFalse(holds_key(b'api_key = "per-trial-token"', KEY))


if __name__ == '__main__':
    unittest.main()
