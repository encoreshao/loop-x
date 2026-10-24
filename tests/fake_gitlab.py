"""A tiny fake GitLab REST server for black-box CLI contract tests.

`routes` maps a decoded request path (e.g. "/api/v4/projects/grp/alpha/issues")
to the full list of items it serves; pagination (`page`/`per_page`) is applied
like GitLab does. Every request is recorded in `requests`.
"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse


class FakeGitLab:
    def __init__(self, routes):
        self.routes = routes
        self.post_replies = {}
        self.requests = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                parsed = urlparse(self.path)
                path = unquote(parsed.path)
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length) or b"null")
                outer.requests.append({
                    "method": "POST", "path": path, "body": body,
                    "token": self.headers.get("PRIVATE-TOKEN"),
                })
                reply = outer.post_replies.get(path, {"id": 999, "body": (body or {}).get("body")})
                data = json.dumps(reply).encode()
                self.send_response(201)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_PUT(self):
                parsed = urlparse(self.path)
                path = unquote(parsed.path)
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length) or b"null")
                outer.requests.append({
                    "method": "PUT", "path": path, "body": body,
                    "token": self.headers.get("PRIVATE-TOKEN"),
                })
                data = json.dumps({"ok": True}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                parsed = urlparse(self.path)
                query = {k: v[0] for k, v in parse_qs(parsed.query).items()}
                path = unquote(parsed.path)
                outer.requests.append({
                    "path": path, "query": query,
                    "token": self.headers.get("PRIVATE-TOKEN"),
                })
                if path not in outer.routes:
                    body, status = json.dumps({"message": "404 Not Found"}).encode(), 404
                else:
                    items = outer.routes[path]
                    if isinstance(items, list):
                        per = int(query.get("per_page", 20))
                        page = int(query.get("page", 1))
                        items = items[(page - 1) * per: page * per]
                    body, status = json.dumps(items).encode(), 200
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self._server.server_address[1]}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._server.shutdown()
        self._server.server_close()
