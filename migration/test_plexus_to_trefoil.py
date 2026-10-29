"""Tests for plexus_to_trefoil.py using in-process fake Plexus/Trefoil servers.

Run: python3 -m unittest migration/test_plexus_to_trefoil.py
"""

import io
import json
import os
import sys
import tarfile
import tempfile
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from http.server import BaseHTTPRequestHandler, HTTPServer

sys.path.insert(0, os.path.dirname(__file__))
import plexus_to_trefoil as m  # noqa: E402

DATA = {
    "providers": {"acme": {"api_key": "sk-1"}},
    "models": {"fast": {}},
    "keys": {"team": {"secret": "s"}},
    "user_quotas": {"q": {}},
    "mcp_servers": {"docs": {}},
    "mcp_keys": [{"serverName": "docs", "key": "k", "isActive": True}],
    "settings": {"a": 1},
    "oauth_credentials": [{"provider_type": "p", "account_id": "a"}],
}


def envelope(data):
    return {"plexus_backup": True, "version": 1, "data": data}


class Fake:
    """A tiny management API. `store` is the instance's config `data`."""

    def __init__(self, admin_key, data=None, restore_ok=True, drop=()):
        self.admin_key, self.store, self.restore_ok, self.drop = admin_key, data or {}, restore_ok, drop
        self.requests = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _reply(self, code, body, ctype="application/json"):
                raw = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(code)
                self.send_header("content-type", ctype)
                self.end_headers()
                self.wfile.write(raw)

            def _auth(self):
                if self.headers.get("x-admin-key") != fake.admin_key:
                    self._reply(401, {"error": "Unauthorized"})
                    return False
                return True

            def do_GET(self):
                fake.requests.append(("GET", self.path))
                if not self._auth():
                    return
                if self.path.startswith("/v0/management/backup"):
                    if "full=true" in self.path:
                        self._reply(200, make_archive(envelope(fake.store)), "application/gzip")
                    else:
                        self._reply(200, envelope(fake.store))

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("content-length", 0)))
                fake.requests.append(("POST", self.path, self.headers.get("content-type"), body))
                if not self._auth():
                    return
                if not fake.restore_ok:
                    self._reply(400, {"error": "Invalid backup: invalid models"})
                    return
                if body[:2] == b"\x1f\x8b":
                    with tarfile.open(fileobj=io.BytesIO(body), mode="r:gz") as tar:
                        data = json.load(tar.extractfile("config.json"))["data"]
                else:
                    data = json.loads(body)["data"]
                fake.store = {k: v for k, v in data.items() if k not in fake.drop}
                self._reply(200, {"success": True})

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def make_archive(env):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for name, payload in (("manifest.json", {"plexus_backup": True, "version": 1}),
                              ("config.json", env)):
            raw = json.dumps(payload).encode()
            info = tarfile.TarInfo(name)
            info.size = len(raw)
            tar.addfile(info, io.BytesIO(raw))
    return buf.getvalue()


def run(*argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        try:
            code = m.main(list(argv))
        except SystemExit as exit_:  # argparse errors
            code = exit_.code
    return code, out.getvalue(), err.getvalue()


class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.plexus = Fake("pk", DATA)
        self.trefoil = Fake("tk")
        self.addCleanup(self.plexus.close)
        self.addCleanup(self.trefoil.close)

    def base(self, *extra):
        return run("--plexus-url", self.plexus.url, "--plexus-admin-key", "pk",
                   "--trefoil-url", self.trefoil.url, "--trefoil-admin-key", "tk", *extra)

    def test_migrates_and_verifies_every_section(self):
        code, out, err = self.base()
        self.assertEqual(code, 0, err)
        self.assertEqual(self.trefoil.store, DATA)
        self.assertIn("Migration complete and verified.", out)
        post = [r for r in self.trefoil.requests if r[0] == "POST"][0]
        self.assertEqual(post[2], "application/json")

    def test_dry_run_does_not_touch_trefoil(self):
        code, out, _ = self.base("--dry-run")
        self.assertEqual(code, 0)
        self.assertFalse([r for r in self.trefoil.requests if r[0] == "POST"])
        self.assertIn("providers=1", out)

    def test_refuses_populated_target_without_yes(self):
        self.trefoil.store = {"providers": {"old": {}}}
        code, _, err = self.base()
        self.assertEqual(code, 1)
        self.assertIn("--yes", err)
        self.assertEqual(self.trefoil.store, {"providers": {"old": {}}})
        code, _, _ = self.base("--yes")
        self.assertEqual(code, 0)
        self.assertEqual(self.trefoil.store, DATA)

    def test_verification_catches_silently_dropped_sections(self):
        self.trefoil.close()
        self.trefoil = Fake("tk", drop=("mcp_servers", "oauth_credentials"))
        self.addCleanup(self.trefoil.close)
        code, _, err = self.base()
        self.assertEqual(code, 1)
        self.assertIn("mcp_servers: expected 1", err)
        self.assertIn("oauth_credentials: expected 1", err)

    def test_restore_error_is_reported(self):
        self.trefoil.restore_ok = False
        code, _, err = self.base()
        self.assertEqual(code, 1)
        self.assertIn("invalid models", err)

    def test_wrong_plexus_key_fails(self):
        code, _, err = run("--plexus-url", self.plexus.url, "--plexus-admin-key", "nope",
                           "--trefoil-url", self.trefoil.url, "--trefoil-admin-key", "tk")
        self.assertEqual(code, 1)
        self.assertIn("401", err)

    def test_with_history_uses_full_archive(self):
        code, out, err = self.base("--with-history")
        self.assertEqual(code, 0, err)
        post = [r for r in self.trefoil.requests if r[0] == "POST"][0]
        self.assertEqual(post[2], "application/gzip")
        self.assertEqual(self.trefoil.store, DATA)

    def test_backup_file_and_save_backup_permissions(self):
        with tempfile.TemporaryDirectory() as tmp:
            saved = os.path.join(tmp, "b.json")
            code, _, err = self.base("--save-backup", saved)
            self.assertEqual(code, 0, err)
            self.assertEqual(oct(os.stat(saved).st_mode & 0o777), "0o600")
            self.trefoil.store = {}
            code, _, err = run("--backup-file", saved, "--trefoil-url", self.trefoil.url,
                               "--trefoil-admin-key", "tk")
            self.assertEqual(code, 0, err)
            self.assertEqual(self.trefoil.store, DATA)

    def test_rejects_non_plexus_backup(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = os.path.join(tmp, "x.json")
            with open(bad, "w") as handle:
                json.dump({"hello": "world"}, handle)
            code, _, err = run("--backup-file", bad, "--trefoil-url", self.trefoil.url,
                               "--trefoil-admin-key", "tk")
            self.assertEqual(code, 1)
            self.assertIn("not a Plexus config backup", err)

    def test_requires_exactly_one_source(self):
        code, _, _ = run("--trefoil-url", self.trefoil.url, "--trefoil-admin-key", "tk")
        self.assertNotEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
