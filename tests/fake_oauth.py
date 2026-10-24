"""A fake OAuth provider (authorize + token endpoints) for black-box tests.

GET /auth validates the request like a real provider would (client_id, redirect_uri,
PKCE challenge when sent) and 302s to `redirect_uri?code=..&state=..` (or `error=` when
`deny` is set). POST /token handles `authorization_code` (verifying the PKCE verifier and
client secret) and `refresh_token`. Every request is recorded in `requests`.
"""
import base64
import hashlib
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse


def s256(verifier):
    return base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()


class FakeOAuth:
    def __init__(self, client_id="cid", client_secret="csecret", expires_in=3600, give_refresh_token=True):
        self.client_id, self.client_secret = client_id, client_secret
        self.expires_in = expires_in
        self.give_refresh_token = give_refresh_token
        self.deny = False
        self.requests = []
        self.codes = {}          # code -> {"challenge", "redirect_uri"}
        self.refresh_tokens = set()
        self.issued = 0
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _reply(self, status, doc, headers=()):
                data = json.dumps(doc).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                for k, v in headers:
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                parsed = urlparse(self.path)
                q = {k: v[0] for k, v in parse_qs(parsed.query).items()}
                outer.requests.append({"method": "GET", "path": parsed.path, "query": q})
                if parsed.path != "/auth" or q.get("client_id") != outer.client_id or q.get("response_type") != "code":
                    return self._reply(400, {"error": "invalid_request"})
                if "code_challenge" in q and q.get("code_challenge_method") != "S256":
                    return self._reply(400, {"error": "invalid_request"})
                if outer.deny:
                    loc = q["redirect_uri"] + "?" + urlencode({"error": "access_denied", "state": q["state"]})
                else:
                    code = f"code-{len(outer.codes) + 1}"
                    outer.codes[code] = {"challenge": q.get("code_challenge"), "redirect_uri": q["redirect_uri"]}
                    loc = q["redirect_uri"] + "?" + urlencode({"code": code, "state": q["state"]})
                self.send_response(302)
                self.send_header("Location", loc)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                form = {k: v[0] for k, v in parse_qs(self.rfile.read(length).decode()).items()}
                outer.requests.append({"method": "POST", "path": self.path, "form": form})
                if self.path != "/token" or form.get("client_id") != outer.client_id \
                        or form.get("client_secret") != outer.client_secret:
                    return self._reply(401, {"error": "invalid_client"})
                grant = form.get("grant_type")
                if grant == "authorization_code":
                    info = outer.codes.pop(form.get("code"), None)
                    if info is None or info["redirect_uri"] != form.get("redirect_uri"):
                        return self._reply(400, {"error": "invalid_grant"})
                    if info["challenge"] and s256(form.get("code_verifier", "")) != info["challenge"]:
                        return self._reply(400, {"error": "invalid_grant"})
                    outer.issued += 1
                    doc = {"access_token": f"at-{outer.issued}", "expires_in": outer.expires_in,
                           "token_type": "Bearer", "scope": "fake"}
                    if outer.give_refresh_token:
                        doc["refresh_token"] = "rt-1"
                        outer.refresh_tokens.add("rt-1")
                    return self._reply(200, doc)
                if grant == "refresh_token":
                    if form.get("refresh_token") not in outer.refresh_tokens:
                        return self._reply(400, {"error": "invalid_grant"})
                    outer.issued += 1
                    return self._reply(200, {"access_token": f"at-{outer.issued}", "expires_in": outer.expires_in})
                return self._reply(400, {"error": "unsupported_grant_type"})

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self._server.server_address[1]}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def grants(self, grant):
        return [r for r in self.requests if r["method"] == "POST" and r["form"].get("grant_type") == grant]

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._server.shutdown()
        self._server.server_close()
