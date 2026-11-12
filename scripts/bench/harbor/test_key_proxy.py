"""Real sockets on 127.0.0.1: a fake plain-HTTP upstream behind the real proxy."""
import http.client
import http.server
import json
import logging
import socket
import threading
import unittest

from key_proxy import KeyProxy, holds_key

KEY = 'prx-fixture-upstream-key-not-exported'


class Upstream(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, format, *args):
        del format, args

    def do_GET(self):
        self.do_POST()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        self.server.seen.append({'path': self.path, 'headers': dict(self.headers.items()), 'body': body})
        if self.path.endswith('/stream'):
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Transfer-Encoding', 'chunked')
            self.end_headers()
            self.wfile.write(b'7\r\ndata: 1\r\n')
            self.wfile.flush()
            # Hold the reply open until the client has read the first event.
            self.server.first_read.wait(timeout=5)
            self.wfile.write(b'7\r\ndata: 2\r\n0\r\n\r\n')
            return
        if self.path.endswith('/denied'):
            reply, status = json.dumps({'error': 'bad key ' + KEY}).encode(), 401
        else:
            reply, status = json.dumps({'ok': True}).encode(), 200
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Set-Cookie', 'upstream=1')
        self.send_header('Content-Length', str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)


class KeyProxyTests(unittest.TestCase):
    def setUp(self):
        self.upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        self.upstream.daemon_threads = True
        self.upstream.seen = []
        self.upstream.first_read = threading.Event()
        threading.Thread(target=self.upstream.serve_forever, args=(0.05,), daemon=True).start()
        self.addCleanup(self.upstream.server_close)
        self.addCleanup(self.upstream.shutdown)
        port = self.upstream.server_address[1]
        self.proxy = KeyProxy(f'http://127.0.0.1:{port}/api/v1', KEY, '127.0.0.1',
                              allow_http_upstream=True, max_body=1024)
        self.proxy.start()
        self.addCleanup(self.proxy.stop)

    def request(self, method, path, body=None, token=None, headers=None):
        connection = http.client.HTTPConnection(*self.proxy.address, timeout=10)
        self.addCleanup(connection.close)
        sent = dict(headers or {})
        if token is not None:
            sent['Authorization'] = 'Bearer ' + token
        connection.request(method, path, body=body, headers=sent)
        return connection.getresponse()

    def test_wrong_or_missing_token_is_401_without_upstream_call(self):
        for token in (None, 'wrong', KEY, self.proxy.token + 'x', ''):
            with self.subTest(token=token):
                response = self.request('POST', '/api/v1/chat/completions', b'{}', token)
                self.assertEqual(response.status, 401)
                self.assertEqual(response.read(), b'')
        self.assertEqual(self.upstream.seen, [])

    def test_right_token_is_swapped_for_real_key(self):
        with self.assertLogs('prx_bench.key_proxy', logging.INFO) as logs:
            response = self.request('POST', '/api/v1/chat/completions?trace=query-fixture', b'{"model":"m"}',
                                    self.proxy.token, {'Content-Type': 'application/json', 'X-Extra': 'dropped',
                                                       'Cookie': 'c=1'})
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(response.read()), {'ok': True})
        self.assertIsNone(response.getheader('Set-Cookie'), 'only allowlisted response headers pass')
        (seen,) = self.upstream.seen
        self.assertEqual(seen['headers']['Authorization'], 'Bearer ' + KEY)
        self.assertEqual(seen['body'], b'{"model":"m"}')
        self.assertEqual(seen['headers']['Host'], '127.0.0.1:%d' % self.upstream.server_address[1])
        self.assertNotIn('X-Extra', seen['headers'])
        self.assertNotIn('Cookie', seen['headers'])
        self.assertNotIn(self.proxy.token, json.dumps(seen['headers']) + seen['path'])
        text = '\n'.join(logs.output)
        self.assertIn('POST /api/v1/chat/completions 200 in=13 out=12', text)
        for hidden in (KEY, KEY[:8], self.proxy.token, 'query-fixture', 'model'):
            self.assertNotIn(hidden, text)

    def test_paths_outside_prefix_and_foreign_hosts_are_refused(self):
        targets = ('/', '/api/v2/chat', '/api/v1x', '/api/v1/../admin', '/api/v1/%2e%2e/admin',
                   '/api/v1/%2Fadmin', '//evil.example/api/v1/chat', 'http://evil.example/api/v1/chat',
                   '/other?next=/api/v1/chat', '/api/v1\\..\\admin')
        for target in targets:
            with self.subTest(target=target):
                response = self.request('POST', target, b'{}', self.proxy.token)
                self.assertEqual(response.status, 404)
        self.assertEqual(self.upstream.seen, [])

    def test_streamed_response_passes_through_before_upstream_finishes(self):
        response = self.request('POST', '/api/v1/stream', b'{}', self.proxy.token)
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader('Content-Type'), 'text/event-stream')
        # The upstream blocks after this event until first_read is set: a
        # buffering proxy would time out here instead of returning it.
        self.assertEqual(response.read1(64), b'data: 1')
        self.upstream.first_read.set()
        self.assertEqual(response.read(), b'data: 2')

    def test_oversized_or_unbounded_body_is_refused_before_upstream(self):
        with socket.create_connection(self.proxy.address, timeout=5) as raw:
            # Declared length only: the proxy must refuse before reading a body.
            raw.sendall(b'POST /api/v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer '
                        + self.proxy.token.encode() + b'\r\nContent-Length: 1025\r\n\r\n')
            self.assertTrue(raw.recv(64).startswith(b'HTTP/1.1 413'))
        with socket.create_connection(self.proxy.address, timeout=5) as raw:
            raw.sendall(b'POST /api/v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer '
                        + self.proxy.token.encode() + b'\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n\r\n')
            self.assertTrue(raw.recv(64).startswith(b'HTTP/1.1 411'))
        self.assertEqual(self.upstream.seen, [])

    def test_upstream_error_body_is_scrubbed(self):
        response = self.request('POST', '/api/v1/denied', b'{}', self.proxy.token)
        self.assertEqual(response.status, 401)
        body = response.read()
        self.assertFalse(holds_key(body, KEY))
        self.assertIn(b'[redacted]', body)

    def test_stop_kills_the_token_and_the_listener(self):
        address = self.proxy.address
        self.proxy.stop()
        with self.assertRaises(OSError):
            socket.create_connection(address, timeout=2).close()

    def test_unreachable_upstream_is_a_bare_502(self):
        self.upstream.shutdown()
        self.upstream.server_close()
        response = self.request('POST', '/api/v1/chat/completions', b'{}', self.proxy.token)
        self.assertEqual(response.status, 502)
        self.assertEqual(response.read(), b'')

    def test_constructor_refuses_unsafe_upstreams_and_keys(self):
        for upstream in ('http://127.0.0.1:1/v1', 'https://u:p@host.example/v1', 'https://host.example',
                         'https://host.example/', 'https://host.example/v1?x=1', 'ftp://host.example/v1',
                         'https://host.example/v1/../x'):
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
