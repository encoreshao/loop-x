"""Local HTTP stub for provider/auth tests: replays queued JSON responses
per (method, path) and records every request. Real sockets, no mocks of
the code under test."""
import json
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class StubServer:
    def __init__(self):
        self.routes = {}
        self.requests = []

    def add(self, method, path, status=200, body=None, headers=None):
        self.routes.setdefault((method, path), []).append((status, body, headers or {}))
        return self

    def __enter__(self):
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def _handle(self):
                split = urllib.parse.urlsplit(self.path)
                length = int(self.headers.get("Content-Length", 0) or 0)
                raw = self.rfile.read(length) if length else b""
                stub.requests.append({
                    "method": self.command, "path": split.path,
                    "query": urllib.parse.parse_qs(split.query),
                    "headers": dict(self.headers), "body": raw.decode("utf-8", "replace"),
                })
                queue = stub.routes.get((self.command, split.path))
                if not queue:
                    status, body, headers = 404, {"error": "no stub"}, {}
                else:
                    status, body, headers = queue.pop(0) if len(queue) > 1 else queue[0]
                payload = b"" if body is None else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                for key, value in headers.items():
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            do_GET = do_POST = do_PATCH = do_DELETE = _handle

            def log_message(self, *args):
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.base_url = f"http://127.0.0.1:{self._server.server_address[1]}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._server.shutdown()
        self._thread.join(timeout=5)
        self._server.server_close()

    def bodies(self, method, path):
        return [r["body"] for r in self.requests if r["method"] == method and r["path"] == path]
