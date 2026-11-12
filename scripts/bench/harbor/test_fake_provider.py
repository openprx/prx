import json
import socket
import threading
import unittest

from fake_provider import Handler, MAX_BODY, MAX_HEAD, Server


class FakeProviderTests(unittest.TestCase):
    def exchange(self, request):
        with Server(('127.0.0.1', 0), Handler) as server:
            server.accepted = 0
            server.timeout = 5
            thread = threading.Thread(target=server.handle_request, daemon=True)
            thread.start()
            with socket.create_connection(server.server_address, timeout=2) as client:
                client.settimeout(6)
                client.sendall(request)
                client.shutdown(socket.SHUT_WR)
                data = bytearray()
                while len(data) < MAX_HEAD:
                    chunk = client.recv(min(4096, MAX_HEAD - len(data)))
                    if not chunk:
                        break
                    data.extend(chunk)
            thread.join(timeout=6)
            self.assertFalse(thread.is_alive(), 'fake provider exceeded socket deadline')
            self.assertEqual(server.accepted, 1, 'every connection must count')
            return bytes(data)

    def test_real_wire_reply(self):
        body = json.dumps({'model': 'prx-fixture'}).encode()
        response = self.exchange(b'POST /v1/chat/completions HTTP/1.1\r\nHost: fixture\r\nContent-Length: '
                                 + str(len(body)).encode() + b'\r\n\r\n' + body)
        head, payload = response.split(b'\r\n\r\n', 1)
        self.assertIn(b'200', head)
        self.assertEqual(json.loads(payload)['model'], 'prx-fixture')

    def test_input_body_overflow_rejected_without_allocation(self):
        response = self.exchange(b'POST /v1/chat/completions HTTP/1.1\r\nContent-Length: '
                                 + str(MAX_BODY + 1).encode() + b'\r\n\r\n')
        self.assertEqual(response, b'', 'oversized body must be rejected before reading')
