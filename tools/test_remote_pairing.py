#!/usr/bin/env python3
"""Tests for the phone remote's pairing gate and settings proxy (files/pifin-remote/server.py).
No Pi: evdev is stubbed, the token file and the daemon socket live in a temp dir.
Run: python3 tools/test_remote_pairing.py"""
import http.client
import json
import os
import pathlib
import socket
import sys
import tempfile
import threading
import types
import unittest
from unittest import mock
from http.server import ThreadingHTTPServer

sys.dont_write_bytecode = True
ROOT = pathlib.Path(__file__).resolve().parent.parent
TMP = tempfile.mkdtemp(prefix="pair")
TOKEN_FILE = os.path.join(TMP, "pair-token")
SOCK = os.path.join(TMP, "settings.sock")
os.environ["PIFIN_PAIR_TOKEN"] = TOKEN_FILE
os.environ["PIFIN_SETTINGS_SOCK"] = SOCK
os.environ["PIFIN_AUTH"] = os.path.join(TMP, "no-auth.json")

# evdev only exists on the Pi; the remote just needs UInput and key codes
evdev = types.ModuleType("evdev")
evdev.UInput = lambda *a, **k: types.SimpleNamespace(write=lambda *a: None, syn=lambda: None)
evdev.ecodes = types.SimpleNamespace(EV_KEY=1, KEY_UP=103, KEY_DOWN=108, KEY_LEFT=105, KEY_RIGHT=106,
                                     KEY_ENTER=28, KEY_ESC=1, KEY_BACKSPACE=14)
sys.modules["evdev"] = evdev
sys.path.insert(0, str(ROOT / "files" / "pifin-remote"))
import server  # noqa: E402

SENTENCE = b"Scan the QR code on the TV (Settings > System > Phone remote)."
GOOD = "0123456789abcdef0123456789abcdef"


def write_token(value):
    # atomic replace, like the daemon's pair_reset
    tmp = TOKEN_FILE + ".tmp"
    pathlib.Path(tmp).write_text(value + "\n")
    os.replace(tmp, TOKEN_FILE)


class FakeDaemon:
    """A settings socket that records each request and answers one fixed line."""

    def __init__(self):
        self.requests = []
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.bind(SOCK)
        self.sock.listen()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            with conn:
                line = conn.makefile("rb").readline()
                self.requests.append(json.loads(line))
                conn.sendall(b'{"ok": true, "result": {"settings": {"version": 1}, "status": {}}}\n')

    def close(self):
        self.sock.close()
        os.unlink(SOCK)


class Pairing(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()

    def setUp(self):
        write_token(GOOD)

    def req(self, method, path, cookie=None, body=None):
        c = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=10)
        headers = {"Cookie": f"pifin={cookie}"} if cookie else {}
        data = json.dumps(body).encode() if body is not None else None
        if data is not None:
            headers["Content-Type"] = "application/json"
        c.request(method, path, body=data, headers=headers)
        r = c.getresponse()
        out = (r.status, r.getheader("Set-Cookie"), r.read())
        c.close()
        return out

    def test_api_needs_the_cookie(self):
        code, _, body = self.req("GET", "/api/status")
        self.assertEqual(code, 403)
        self.assertIn(SENTENCE, body)
        self.assertEqual(self.req("GET", "/api/status", cookie="wrong" + GOOD[5:])[0], 403)
        self.assertEqual(self.req("GET", "/api/status", cookie=GOOD)[0], 200)
        # pages too, and POSTs
        self.assertEqual(self.req("GET", "/")[0], 403)
        self.assertEqual(self.req("GET", "/settings")[0], 403)
        self.assertEqual(self.req("POST", "/api/action", body={"action": "up"})[0], 403)
        self.assertEqual(self.req("GET", "/settings", cookie=GOOD)[0], 200)
        # the icon and manifest stay public
        self.assertEqual(self.req("GET", "/icon.svg")[0], 200)

    def test_pair_sets_the_cookie_only_for_the_right_token(self):
        code, cookie, body = self.req("GET", "/pair?t=" + GOOD)
        self.assertEqual(code, 200)
        for flag in (f"pifin={GOOD}", "HttpOnly", "SameSite=Strict", "Max-Age=31536000", "Path=/"):
            self.assertIn(flag, cookie)
        self.assertIn(b'http-equiv="refresh" content="0;url=/"', body)
        for bad in ("bad", GOOD[:8], GOOD + "00", ""):
            code, cookie, body = self.req("GET", "/pair?t=" + bad)
            self.assertEqual((code, cookie), (403, None), bad)
            self.assertIn(SENTENCE, body)
        self.assertEqual(self.req("GET", "/pair")[0], 403)

    def test_no_token_file_locks_everyone_out(self):
        os.unlink(TOKEN_FILE)
        self.assertEqual(self.req("GET", "/api/status", cookie=GOOD)[0], 403)
        self.assertEqual(self.req("GET", "/pair?t=" + GOOD)[0], 403)

    def test_repair_logs_the_old_cookie_out(self):
        self.assertEqual(self.req("GET", "/api/status", cookie=GOOD)[0], 200)
        new = "f" * 32
        write_token(new)
        self.assertEqual(self.req("GET", "/api/status", cookie=GOOD)[0], 403)
        self.assertEqual(self.req("GET", "/api/status", cookie=new)[0], 200)

    def test_settings_forwards_whitelisted_ops_only(self):
        fake = FakeDaemon()
        self.addCleanup(fake.close)
        code, _, body = self.req("POST", "/api/settings", cookie=GOOD, body={"op": "get"})
        self.assertEqual(code, 200)
        self.assertEqual(json.loads(body)["result"]["settings"], {"version": 1})
        self.assertEqual(fake.requests, [{"op": "get"}])
        for op in ("pair_reset", "nope", None):
            code, _, body = self.req("POST", "/api/settings", cookie=GOOD, body={"op": op})
            self.assertEqual(code, 400, op)
            self.assertEqual(json.loads(body)["error"], "bad_request")
        self.assertEqual(fake.requests, [{"op": "get"}])

    def raw_post(self, path, length):
        c = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=3)
        c.putrequest("POST", path)
        c.putheader("Cookie", f"pifin={GOOD}")
        c.putheader("Content-Length", length)
        c.endheaders()
        r = c.getresponse()  # times out if the server waits for a body that never comes
        out = r.status
        r.read()
        c.close()
        return out

    def test_bad_content_length_is_refused(self):
        for length in ("-1", "abc"):
            self.assertEqual(self.raw_post("/api/action", length), 400, length)
            self.assertEqual(self.raw_post("/api/settings", length), 400, length)

    def test_pair_cookie_is_the_validated_token(self):
        reads = iter([GOOD] + ["f" * 32] * 10)  # the file changes right after the check
        with mock.patch.object(server, "pair_token", lambda: next(reads)):
            code, cookie, _ = self.req("GET", "/pair?t=" + GOOD)
        self.assertEqual(code, 200)
        self.assertIn(f"pifin={GOOD};", cookie)

    def test_settings_service_missing(self):
        code, _, body = self.req("POST", "/api/settings", cookie=GOOD, body={"op": "get"})
        self.assertEqual(code, 200)
        self.assertEqual(json.loads(body), {"ok": False, "error": "failed", "message": "Settings service not running."})


class PhoneGone(unittest.TestCase):
    """The TV gets "unseen" only once every phone has left the remote page."""

    def setUp(self):
        self.sent = []
        patch = mock.patch.object(server, "app_cmd", side_effect=lambda c: self.sent.append(c) or "ok")
        patch.start()
        self.addCleanup(patch.stop)
        self.addCleanup(server.state.update, dict(server.state))
        server.state.update(seen=False, phones={}, bye_at=None)

    def test_sleeping_phone_keeps_the_qr_hidden(self):
        server.seen_now(100.0, "A")
        for t in range(100, 100 + 3 * 3600, 60):      # a film long, no request at all
            server.check_gone(float(t))
        self.assertEqual(self.sent, ["seen"])

    def test_leaving_brings_the_qr_back_once_after_the_grace(self):
        server.seen_now(100.0, "A")
        server.bye(110.0, "A")
        server.check_gone(110.0 + server.BYE_GRACE - 1)  # still in the grace
        self.assertEqual(self.sent, ["seen"])
        server.check_gone(110.0 + server.BYE_GRACE)
        server.check_gone(200.0)                          # told once, not every 2 s
        self.assertEqual(self.sent, ["seen", "unseen"])
        server.seen_now(300.0, "A")                       # back: the QR hides again
        self.assertEqual(self.sent, ["seen", "unseen", "seen"])

    def test_reload_or_settings_page_comes_back_within_the_grace(self):
        server.seen_now(100.0, "A")
        server.bye(110.0, "A")
        server.seen_now(111.0, "A")
        server.check_gone(130.0)
        self.assertEqual(self.sent, ["seen"])

    def test_qr_waits_for_the_last_phone(self):
        server.seen_now(100.0, "A")
        server.seen_now(101.0, "B")
        server.bye(110.0, "A")
        server.check_gone(130.0)
        self.assertEqual(self.sent, ["seen"])
        server.bye(140.0, "B")
        server.check_gone(160.0)
        self.assertEqual(self.sent, ["seen", "unseen"])

    def test_phone_silent_for_hours_is_dropped(self):
        server.seen_now(100.0, "A")
        server.check_gone(100.0 + server.STALE_AFTER + 1)
        self.assertEqual(self.sent, ["seen", "unseen"])

    def test_no_phone_ever_means_no_unseen(self):
        server.check_gone(100_000.0)
        self.assertEqual(self.sent, [])

    def test_bye_endpoint_needs_pairing_and_records_the_phone(self):
        write_token(GOOD)
        server.seen_now(1.0, "127.0.0.1")
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        c.request("POST", "/api/bye")
        self.assertEqual(c.getresponse().status, 403)
        self.assertIn("127.0.0.1", server.state["phones"])
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        c.request("POST", "/api/bye", headers={"Cookie": "pifin=" + GOOD})
        self.assertEqual(c.getresponse().status, 200)
        self.assertNotIn("127.0.0.1", server.state["phones"])
        self.assertIsNotNone(server.state["bye_at"])

    @classmethod
    def setUpClass(cls):
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
        cls.port = cls.httpd.server_address[1]
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.httpd.server_close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
