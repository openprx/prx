"""Bounded stdlib OpenAI-compatible fake provider for unpaid QA smoke only."""
import argparse
import json
import socketserver
import time

MAX_HEAD = 16 * 1024
MAX_BODY = 4 * 1024 * 1024
MAX_CONNECTIONS = 64
SOCKET_TIMEOUT = 5


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(SOCKET_TIMEOUT)
        end = time.monotonic() + SOCKET_TIMEOUT
        try:
            head = bytearray()
            while not head.endswith(b'\r\n\r\n'):
                if len(head) >= MAX_HEAD or time.monotonic() >= end:
                    return
                byte = self.request.recv(1)
                if not byte:
                    return
                head.extend(byte)
            lines = head.decode('ascii').split('\r\n')
            method, path, _ = lines[0].split()
            headers = dict(line.split(':', 1) for line in lines[1:] if line)
            headers = {k.lower(): v.strip() for k, v in headers.items()}
            length = int(headers.get('content-length', '0'))
            if not 0 < length <= MAX_BODY or 'transfer-encoding' in headers:
                return
            body = bytearray()
            while len(body) < length:
                if time.monotonic() >= end:
                    return
                chunk = self.request.recv(min(65536, length - len(body)))
                if not chunk:
                    return
                body.extend(chunk)
            request = json.loads(body)
            if method != 'POST' or not path.endswith('/chat/completions'):
                code, response = 404, {'error': {'message': 'unexpected method or path'}}
            else:
                code, response = 200, {'id': 'prx-smoke', 'object': 'chat.completion',
                    'model': request.get('model'), 'choices': [{'index': 0, 'finish_reason': 'stop',
                    'message': {'role': 'assistant', 'content': 'PRX unpaid smoke complete.'}}],
                    'usage': {'prompt_tokens': 20, 'completion_tokens': 5, 'total_tokens': 25,
                              'prompt_tokens_details': {'cached_tokens': 0}}}
            response = json.dumps(response).encode()
            self.request.sendall((f'HTTP/1.1 {code} Fixture\r\nContent-Type: application/json\r\n'
                f'Content-Length: {len(response)}\r\nConnection: close\r\n\r\n').encode() + response)
        except (ValueError, OSError, KeyError, TypeError):
            # Malformed smoke requests are closed without reflecting credentials.
            return


class Server(socketserver.TCPServer):
    def get_request(self):
        pair = super().get_request()
        self.accepted += 1
        return pair


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bind', required=True)
    parser.add_argument('--port', type=int, default=0)
    parser.add_argument('--deadline-sec', type=float, default=1200)
    args = parser.parse_args()
    if not 0 < args.deadline_sec <= 3600:
        parser.error('deadline must be in (0, 3600]')
    with Server((args.bind, args.port), Handler) as server:
        server.timeout = 1
        server.accepted = 0
        print(server.server_address[1], flush=True)
        deadline = time.monotonic() + args.deadline_sec
        while server.accepted < MAX_CONNECTIONS and time.monotonic() < deadline:
            server.handle_request()


if __name__ == '__main__':
    main()
