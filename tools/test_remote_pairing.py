#!/usr/bin/env python3
"""Tests for the phone remote (files/pifin-remote/server.py): pairing gate, settings proxy,
security headers, artwork types, the server test's address rules and the connection cap.
No Pi: evdev is stubbed, the token file and the daemon socket live in a temp dir.
Run: python3 tools/test_remote_pairing.py"""
import http.client
import json
import os
import pathlib
import re
import select
import socket
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest import mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

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
        cls.httpd = server.RemoteServer(("127.0.0.1", 0), server.Handler)
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
        self.assertEqual(fake.requests, [{"op": "get", "via": "phone"}])
        for op in ("pair_reset", "nope", None):
            code, _, body = self.req("POST", "/api/settings", cookie=GOOD, body={"op": op})
            self.assertEqual(code, 400, op)
            self.assertEqual(json.loads(body)["error"], "bad_request")
        self.assertEqual(fake.requests, [{"op": "get", "via": "phone"}])
        # a phone can't pass itself off as the TV
        for via in ("tv", None, ""):
            fake.requests.clear()
            self.req("POST", "/api/settings", cookie=GOOD, body={"op": "reboot", "via": via})
            self.assertEqual(fake.requests, [{"op": "reboot", "via": "phone"}], via)

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
        cls.httpd = server.RemoteServer(("127.0.0.1", 0), server.Handler)
        cls.port = cls.httpd.server_address[1]
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.httpd.server_close()


class FakeJellyfin:
    """A tiny HTTP server on a loopback address: answers /System/Info/Public, artwork with a
    chosen Content-Type, or a redirect. Counts the requests it got."""

    def __init__(self, host="127.0.0.1", ctype="image/png", redirect=None):
        self.hits = 0
        self.ctype = ctype
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                fake.hits += 1
                if redirect:
                    self.send_response(302)
                    self.send_header("Location", redirect)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if self.path.startswith("/System/Info/Public"):
                    body, ctype = json.dumps({"ServerName": "Fake", "Version": "10.10.7"}).encode(), "application/json"
                else:
                    body, ctype = b"<html><script>alert(1)</script></html>", fake.ctype
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.httpd = ThreadingHTTPServer((host, 0), H)
        self.url = f"http://{host}:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class Hardening(unittest.TestCase):
    """Security headers on every reply, artwork limited to images."""

    @classmethod
    def setUpClass(cls):
        cls.httpd = server.RemoteServer(("127.0.0.1", 0), server.Handler)
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.httpd.server_close()

    def setUp(self):
        write_token(GOOD)

    def get(self, path, cookie=GOOD):
        c = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=10)
        c.request("GET", path, headers={"Cookie": f"pifin={cookie}"} if cookie else {})
        r = c.getresponse()
        out = (r.status, r, r.read())
        c.close()
        return out

    def test_every_reply_carries_the_security_headers(self):
        for path, cookie, status in (("/", GOOD, 200), ("/settings", GOOD, 200), ("/api/status", GOOD, 200),
                                     ("/api/status", None, 403), ("/nope", GOOD, 404),
                                     ("/pair?t=" + GOOD, None, 200), ("/icon.svg", None, 200)):
            code, r, _ = self.get(path, cookie)
            self.assertEqual(code, status, path)
            self.assertEqual(r.getheader("X-Content-Type-Options"), "nosniff", path)
            self.assertEqual(r.getheader("Referrer-Policy"), "no-referrer", path)
            csp = {d.split()[0]: d.split()[1:] for d in r.getheader("Content-Security-Policy", "").split("; ")}
            self.assertEqual(csp.get("default-src"), ["'self'"], path)
            self.assertEqual(csp.get("script-src"), ["'self'", "'unsafe-inline'"], path)
            self.assertEqual(csp.get("connect-src"), ["'self'"], path)
            self.assertEqual(csp.get("frame-ancestors"), ["'none'"], path)
            self.assertEqual(csp.get("base-uri"), ["'none'"], path)

    def test_pages_fit_the_csp(self):
        # the CSP allows inline <script>/<style> but no other origin: the pages must not
        # load anything from elsewhere (inline handlers are allowed but kept out on purpose)
        for name, page in (("index", server.INDEX), ("settings", server.SETTINGS_PAGE)):
            html = page.decode()
            self.assertIsNone(re.search(r"<[^>]+\son[a-z]+\s*=", html), name)
            self.assertIsNone(re.search(r"""(src|href|action)\s*=\s*["']?(https?:)?//""", html), name)
            self.assertIsNone(re.search(r"""(fetch|sendBeacon)\(\s*["'`](https?:)?//""", html), name)

    def art(self, ctype):
        fake = FakeJellyfin(ctype=ctype)
        self.addCleanup(fake.close)
        auth = os.path.join(TMP, "auth.json")
        pathlib.Path(auth).write_text(json.dumps({"server": fake.url, "device_id": "d", "token": "t", "user_id": "u"}))
        with mock.patch.object(server, "AUTH", auth):
            code, r, body = self.get("/api/art/abc-123")
        self.assertEqual(fake.hits, 1)
        return code, r.getheader("Content-Type"), body

    def test_art_passes_images_only(self):
        for given, sent in (("image/png", "image/png"), ("IMAGE/JPEG; charset=x", "image/jpeg"),
                            ("image/webp", "image/webp"), ("image/gif", "image/gif")):
            code, ctype, body = self.art(given)
            self.assertEqual((code, ctype), (200, sent), given)
            self.assertIn(b"<html>", body)
        for bad in ("text/html", "text/html; charset=utf-8", "image/svg+xml", "application/xhtml+xml",
                    "text/plain", "application/octet-stream"):
            code, ctype, body = self.art(bad)
            self.assertEqual(code, 404, bad)
            self.assertNotIn(b"<html>", body, bad)


class ServerTest(unittest.TestCase):
    """The setup wizard's server test must not reach the Pi itself or link-local services."""

    def test_address_rules(self):
        # 10.41.0.1 (comitup's hotspot) and 100.100.100.100 (tailnet range) pass the leak check
        for ip in ("172.16.0.9", "10.41.0.1", "100.100.100.100", "198.51.100.7", "2001:db8::1", "fd00::5"):
            self.assertTrue(server.address_allowed(ip), ip)
        for ip in ("127.0.0.1", "127.8.9.1", "::1", "169.254.169.254", "fe80::1%eth0", "0.0.0.0", "::",
                   "224.0.0.1", "ff02::1", "::ffff:127.0.0.1", "::ffff:169.254.169.254"):
            self.assertFalse(server.address_allowed(ip), ip)

    def test_loopback_is_refused_before_connecting(self):
        fake = FakeJellyfin()
        self.addCleanup(fake.close)
        port = fake.httpd.server_address[1]
        for url in (fake.url, f"http://localhost:{port}", f"http://[::ffff:127.0.0.1]:{port}"):
            self.assertEqual(server.server_test(url)["error"], "invalid_value", url)
        self.assertEqual(fake.hits, 0)
        # the same server passes once loopback is allowed: only the address rule refused it
        with mock.patch.object(server, "address_allowed", lambda ip: True):
            self.assertEqual(server.server_test(fake.url), {"ok": True, "result": {"name": "Fake", "version": "10.10.7"}})
        self.assertEqual(fake.hits, 1)

    def test_metadata_address_is_refused_at_once(self):
        start = time.monotonic()
        self.assertEqual(server.server_test("http://169.254.169.254/")["error"], "invalid_value")
        self.assertEqual(server.server_test("http://127.0.0.1:22")["error"], "invalid_value")
        self.assertLess(time.monotonic() - start, 2)

    def test_only_allowed_addresses_of_a_host_are_tried(self):
        # a name with a LAN and a link-local address: the link-local one is skipped, not fatal
        infos = [(socket.AF_INET6, socket.SOCK_STREAM, 6, "", ("fe80::1%lo", 80, 0, 1)),
                 (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("192.0.2.10", 80))]
        tried = []

        class FakeSocket:
            def __init__(self, *a):
                pass

            def settimeout(self, t):
                pass

            def connect(self, addr):
                tried.append(addr[0])

        with mock.patch.object(server.socket, "getaddrinfo", lambda *a, **k: infos), \
                mock.patch.object(server.socket, "socket", FakeSocket):
            server.checked_connection(("mixed.example", 80), 5)
        self.assertEqual(tried, ["192.0.2.10"])

    def test_redirects_are_checked_too(self):
        target = FakeJellyfin("127.0.0.1")
        self.addCleanup(target.close)
        hop = FakeJellyfin("127.0.0.2", redirect=target.url + "/System/Info/Public")
        self.addCleanup(hop.close)
        with mock.patch.object(server, "address_allowed", lambda ip: ip == "127.0.0.2"):
            self.assertEqual(server.server_test(hop.url)["error"], "invalid_value")
        self.assertEqual((hop.hits, target.hits), (1, 0))
        with mock.patch.object(server, "address_allowed", lambda ip: ip.startswith("127.0.0.")):
            self.assertTrue(server.server_test(hop.url)["ok"])
        self.assertEqual((hop.hits, target.hits), (2, 1))


class ConnectionCap(unittest.TestCase):
    """Idle connections can't take every thread: over the cap they are closed at once."""

    def test_idle_connections_cannot_take_every_thread(self):
        class Quick(server.Handler):
            timeout = 3   # the real 30 s, shortened for the test

        class Counting(server.RemoteServer):
            # counts this server's handler threads only
            active = peak = 0
            lock = threading.Lock()

            def process_request_thread(self, *a):
                with self.lock:
                    Counting.active += 1
                    Counting.peak = max(Counting.peak, Counting.active)
                try:
                    super().process_request_thread(*a)
                finally:
                    with self.lock:
                        Counting.active -= 1

        write_token(GOOD)
        httpd = Counting(("127.0.0.1", 0), Quick)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        self.addCleanup(httpd.server_close)
        self.addCleanup(httpd.shutdown)
        idle = [socket.create_connection(httpd.server_address) for _ in range(40)]
        for s in idle:
            self.addCleanup(s.close)
        deadline = time.monotonic() + 2
        while True:
            readable, _, _ = select.select(idle, [], [], 0.1)
            if Counting.active + len(readable) >= 40 or time.monotonic() > deadline:
                break
        self.assertEqual((Counting.active, Counting.peak), (server.MAX_CONNECTIONS, server.MAX_CONNECTIONS))
        # the 8 over the cap got a 503 and were closed, without waiting
        self.assertEqual(len(readable), 40 - server.MAX_CONNECTIONS)
        for s in readable:
            self.assertTrue(s.recv(100).startswith(b"HTTP/1.1 503 "))
        # once the idle ones time out, their slots are free again
        time.sleep(3.5)
        for _ in range(3):
            c = http.client.HTTPConnection("127.0.0.1", httpd.server_address[1], timeout=5)
            c.request("GET", "/api/status", headers={"Cookie": f"pifin={GOOD}"})
            self.assertEqual(c.getresponse().status, 200)
            c.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
