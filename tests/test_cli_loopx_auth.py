"""`loopx auth` and the OAuth routes of `loopx serve`, end to end against a fake OAuth
provider: start -> open the authorization URL -> the provider redirects to the callback on
the real server -> tokens land in the file-backed secret store -> refresh after expiry.

Rust-only; skipped without LOOPX_BIN. Nothing here can reach the real Keychain: every
process gets LOOPX_KEYCHAIN=file:<tmp>.
"""
import json
import os
import socket
import stat
import subprocess
import time
import urllib.error
import urllib.request

import pytest

from cli_runner import loopx_bin
from fake_oauth import FakeOAuth

pytestmark = pytest.mark.skipif(not loopx_bin(), reason="needs LOOPX_BIN")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def oauth():
    with FakeOAuth() as o:
        yield o


@pytest.fixture
def env(tmp_path, oauth):
    home = tmp_path / "home"
    home.mkdir()
    (home / "mail_oauth.json").write_text(json.dumps({
        "google": {"client_id": "cid", "client_secret": "csecret"},
        "linkedin": {"client_id": "cid", "client_secret": "csecret"},
    }))
    return {
        "PATH": "/usr/bin:/bin",
        "HOME": str(tmp_path),
        "LOOP_ENGINEERING_HOME": str(home),
        "LOOPX_ROOT": str(tmp_path / "root"),
        "LOOPX_KEYCHAIN": f"file:{tmp_path / 'keychain'}",
        "LOOPX_OAUTH_GOOGLE_AUTH_URL": oauth.url + "/auth",
        "LOOPX_OAUTH_GOOGLE_TOKEN_URL": oauth.url + "/token",
        "LOOPX_OAUTH_LINKEDIN_AUTH_URL": oauth.url + "/auth",
        "LOOPX_OAUTH_LINKEDIN_TOKEN_URL": oauth.url + "/token",
        "LOOPX_UPDATE_DIR": str(tmp_path / "update"),
    }


@pytest.fixture
def server(env, tmp_path):
    port = free_port()
    proc = subprocess.Popen([loopx_bin(), "serve", "--no-legacy", "--port", str(port)], env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1).read()
            break
        except OSError:
            time.sleep(0.05)
    else:
        proc.kill()
        pytest.fail("loopx serve did not start: " + proc.stderr.read())
    lock = json.loads((tmp_path / "home" / "loopx.lock").read_text())
    yield {"port": port, "token": lock["token"]}
    proc.terminate()
    proc.wait(timeout=10)


def auth(env, *args, check=True):
    r = subprocess.run([loopx_bin(), "auth", *args], env=env, capture_output=True, text=True)
    if check:
        assert r.returncode == 0, r.stderr
    return r


def open_url(url):
    """GET following redirects (like a browser); returns (status, body)."""
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            return resp.status, resp.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def stored_files(tmp_path):
    root = tmp_path / "keychain"
    return [p for p in root.rglob("*") if p.is_file()] if root.exists() else []


def test_google_loopback_flow_end_to_end_with_refresh(env, server, oauth, tmp_path):
    oauth.expires_in = 1  # inside the 60s refresh skew: expired on arrival
    started = json.loads(auth(env, "google", "start", "--account", "cal", "--port", str(server["port"])).stdout)
    assert started["redirect_uri"] == f"http://127.0.0.1:{server['port']}/oauth/google/callback"
    assert started["authorization_url"].startswith(oauth.url + "/auth?")

    status, page = open_url(started["authorization_url"])
    assert status == 200 and "Connected" in page, page
    assert "code-1" not in page and started["state"] not in page

    # The fake saw a PKCE-verified code exchange.
    (exchange,) = oauth.grants("authorization_code")
    assert exchange["form"]["code_verifier"] and exchange["form"]["redirect_uri"] == started["redirect_uri"]

    st = json.loads(auth(env, "google", "status", "--account", "cal").stdout)
    assert st["connected"] and st["has_refresh_token"] and st["expired"]

    # The tokens live in the (file-backed) store, privately, and nowhere else.
    (secret_file,) = stored_files(tmp_path)
    assert stat.S_IMODE(secret_file.stat().st_mode) == 0o600
    assert json.loads(secret_file.read_text())["refresh_token"] == "rt-1"
    assert "at-1" not in auth(env, "google", "status").stdout

    # Refresh after expiry: only when needed, and the grant is a refresh_token one.
    oauth.expires_in = 3600
    r = json.loads(auth(env, "google", "refresh", "--account", "cal", "--if-expired").stdout)
    assert r["refreshed"] is True
    assert len(oauth.grants("refresh_token")) == 1
    r = json.loads(auth(env, "google", "refresh", "--account", "cal", "--if-expired").stdout)
    assert r["refreshed"] is False and len(oauth.grants("refresh_token")) == 1
    assert json.loads(secret_file.read_text())["access_token"] == "at-2"
    assert json.loads(secret_file.read_text())["refresh_token"] == "rt-1"

    # Tokens survive a "restart": a new process still sees the account.
    assert json.loads(auth(env, "google", "status", "--account", "cal").stdout)["connected"]
    auth(env, "google", "logout", "--account", "cal")
    assert not json.loads(auth(env, "google", "status", "--account", "cal").stdout)["connected"]
    assert stored_files(tmp_path) == []


def test_a_state_cannot_be_replayed(env, server, oauth):
    started = json.loads(auth(env, "google", "start", "--port", str(server["port"])).stdout)
    assert open_url(started["authorization_url"])[0] == 200
    cb = f"http://127.0.0.1:{server['port']}/oauth/google/callback?code=code-1&state={started['state']}"
    status, page = open_url(cb)
    assert status == 400 and "expired or was already used" in page


def test_denied_sign_in_shows_a_failure_page_and_stores_nothing(env, server, oauth, tmp_path):
    oauth.deny = True
    started = json.loads(auth(env, "google", "start", "--port", str(server["port"])).stdout)
    status, page = open_url(started["authorization_url"])
    assert status == 400 and "cancelled" in page
    assert stored_files(tmp_path) == []


def test_api_start_needs_authorisation_and_the_app_scheme_comes_back_in_the_page(env, server, oauth):
    base = f"http://127.0.0.1:{server['port']}"

    def post(path, body, headers):
        req = urllib.request.Request(base + path, data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json", **headers})
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read())

    assert post("/api/oauth/google/start", {}, {})[0] == 403
    code, doc = post("/api/oauth/google/start", {"account": "cal", "app_scheme": "loopx-auth"},
                     {"X-LoopX-Token": server["token"]})
    assert code == 200 and set(doc) >= {"authorization_url", "state"}

    pending = json.loads(urllib.request.urlopen(f"{base}/api/oauth/google/status?account=cal&state={doc['state']}").read())
    assert pending["flow"]["state"] == "pending" and pending["connected"] is False

    status, page = open_url(doc["authorization_url"])
    assert status == 200
    assert "loopx-auth://oauth?provider=google&amp;status=ok&amp;account=cal" in page

    done = json.loads(urllib.request.urlopen(f"{base}/api/oauth/google/status?account=cal&state={doc['state']}").read())
    assert done["flow"]["state"] == "done" and done["connected"] is True
    assert "at-" not in json.dumps(done)

    assert post("/api/oauth/google/logout", {"account": "cal"}, {"X-LoopX-Token": server["token"]})[0] == 200
    after = json.loads(urllib.request.urlopen(f"{base}/api/oauth/google/status?account=cal").read())
    assert after["connected"] is False


def test_linkedin_flow_uses_the_plain_callback(env, server, oauth):
    started = json.loads(auth(env, "linkedin", "start", "--port", str(server["port"])).stdout)
    assert started["redirect_uri"] == f"http://127.0.0.1:{server['port']}/oauth/callback"
    assert "code_challenge" not in started["authorization_url"]
    status, page = open_url(started["authorization_url"])
    assert status == 200 and "LinkedIn is connected" in page
    assert json.loads(auth(env, "linkedin", "status", "--account", "default").stdout)["connected"]


def test_missing_client_is_a_clear_error(env, server, tmp_path):
    (tmp_path / "home" / "mail_oauth.json").write_text("{}")
    r = auth(env, "google", "start", "--port", str(server["port"]), check=False)
    assert r.returncode == 1 and "no google OAuth client configured" in r.stderr
