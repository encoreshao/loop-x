"""A fake Slack incoming webhook that records every POST body."""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Webhook:
    def __init__(self, status=200):
        self.posts = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                outer.posts.append({
                    "path": self.path,
                    "body": json.loads(self.rfile.read(length)),
                })
                self.send_response(status)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def texts(self):
        return [p["body"].get("text", "") for p in self.posts]

    def close(self):
        self.server.shutdown()
        self.server.server_close()
