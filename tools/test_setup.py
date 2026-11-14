#!/usr/bin/env python3
"""Tests for the ready-made image's first setup:
  * files/pifin-setup.sh: the install phase reads no secrets and starts nothing in the
    image chroot; the configure phase installs no packages; mode selection.
  * files/pifin-remote/server.py: a fresh PiFin (no server address) opens the wizard,
    the server test, and the update ops the phone may use.
No Pi, no root. Run: python3 tools/test_setup.py"""
import http.client
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import threading
import types
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.dont_write_bytecode = True
ROOT = pathlib.Path(__file__).resolve().parent.parent
SETUP = ROOT / "files" / "pifin-setup.sh"
TMP = tempfile.mkdtemp(prefix="setup")
TOKEN = "0123456789abcdef0123456789abcdef"
pathlib.Path(TMP, "pair-token").write_text(TOKEN + "\n")
os.environ.update(PIFIN_PAIR_TOKEN=os.path.join(TMP, "pair-token"), PIFIN_SETTINGS_SOCK=os.path.join(TMP, "no.sock"),
                  PIFIN_AUTH=os.path.join(TMP, "no-auth.json"), PIFIN_SETTINGS=os.path.join(TMP, "settings.json"))
evdev = types.ModuleType("evdev")
evdev.UInput = lambda *a, **k: types.SimpleNamespace(write=lambda *a: None, syn=lambda: None)
evdev.ecodes = types.SimpleNamespace(EV_KEY=1, KEY_UP=103, KEY_DOWN=108, KEY_LEFT=105, KEY_RIGHT=106,
                                     KEY_ENTER=28, KEY_ESC=1, KEY_BACKSPACE=14)
sys.modules["evdev"] = evdev
sys.path.insert(0, str(ROOT / "files" / "pifin-remote"))
import server  # noqa: E402


def function_body(text, name):
    """The body of a top-level bash function `name() {` ... `}`."""
    start = text.index(f"\n{name}() {{\n")
    end = text.index("\n}\n", start)
    return text[start:end]


class Phases(unittest.TestCase):
    text = SETUP.read_text()

    def code(self, name):
        # comments say what they like; only the commands count
        return "\n".join(ln for ln in function_body(self.text, name).splitlines() if not ln.lstrip().startswith("#"))

    def test_install_reads_no_secrets(self):
        body = self.code("install_phase")
        for word in ("secrets.env", "TS_AUTHKEY", "WIFI_PSK", "WIFI_SSID", "HOTSPOT_PASS", "JELLYFIN_URL",
                     "comitup.conf"):
            self.assertNotIn(word, body, word)

    def test_install_starts_nothing_in_the_image_chroot(self):
        # every restart/reload/start/stop/--now is behind `live`
        for ln in self.code("install_phase").splitlines():
            if re.search(r"systemctl (restart|reload|try-restart|start|stop)|--now|udevadm control", ln):
                self.assertRegex(ln.strip(), r"^(live &&|systemctl reload NetworkManager)|^\s*live && ", ln)
        self.assertIn("if live; then", self.code("install_phase"))

    def test_configure_installs_no_packages(self):
        body = self.code("configure_phase")
        for word in ("apt-get", "curl ", "dpkg -i"):
            self.assertNotIn(word, body, word)

    def run_bash(self, script, root=None):
        env = dict(os.environ, PIFIN_SETUP_SOURCE_ONLY="1", PIFIN_ROOT=root or TMP)
        return subprocess.run(["bash", "-c", f'. "{SETUP}"; {script}'], capture_output=True, text=True, env=env)

    def test_mode_selection(self):
        fresh = tempfile.mkdtemp(dir=TMP)
        self.assertEqual(self.run_bash("pick_mode", fresh).stdout.strip(), "all")  # plain Raspberry Pi OS
        os.makedirs(fresh + "/etc/pifin")
        pathlib.Path(fresh, "etc/pifin/version").write_text("0.3.0\n")
        self.assertEqual(self.run_bash("pick_mode", fresh).stdout.strip(), "configure")  # the image
        self.assertEqual(self.run_bash("pick_mode install", fresh).stdout.strip(), "install")
        self.assertNotEqual(self.run_bash("pick_mode bogus", fresh).returncode, 0)

    def test_sourcing_runs_nothing(self):
        r = self.run_bash("echo sourced")
        self.assertEqual((r.returncode, r.stdout), (0, "sourced\n"))


class FakeJellyfin(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path == "/System/Info/Public":
            body = json.dumps({"ServerName": "Home", "Version": "10.11.0", "Id": "secret-id"}).encode()
            self.send_response(200)
        else:
            body = b"no"
            self.send_response(404)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Wizard(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.httpd = ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
        threading.Thread(target=cls.httpd.serve_forever, daemon=True).start()
        cls.jf = ThreadingHTTPServer(("127.0.0.1", 0), FakeJellyfin)
        threading.Thread(target=cls.jf.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.jf.shutdown()

    def store(self, url):
        pathlib.Path(os.environ["PIFIN_SETTINGS"]).write_text(json.dumps({"version": 1, "system": {"jellyfin_url": url}}))

    def get(self, path):
        c = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=5)
        c.request("GET", path, headers={"Cookie": f"pifin={TOKEN}"})
        r = c.getresponse()
        return r.status, r.read()

    def post(self, path, obj):
        c = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=10)
        body = json.dumps(obj).encode()
        c.request("POST", path, body=body, headers={"Cookie": f"pifin={TOKEN}", "Content-Type": "application/json",
                                                    "Content-Length": str(len(body))})
        r = c.getresponse()
        return r.status, json.loads(r.read())

    def test_fresh_pifin_opens_the_wizard(self):
        self.store("")
        status, body = self.get("/")
        self.assertEqual(status, 200)
        self.assertIn(b'id="wiz"', body)

    def test_configured_pifin_opens_the_remote(self):
        self.store("http://jellyfin.example:8096")
        self.assertNotIn(b'id="wiz"', self.get("/")[1])
        os.unlink(os.environ["PIFIN_SETTINGS"])  # no store at all: the remote too
        self.assertNotIn(b'id="wiz"', self.get("/")[1])

    def test_wizard_needs_pairing(self):
        self.store("")
        c = http.client.HTTPConnection("127.0.0.1", self.httpd.server_address[1], timeout=5)
        c.request("GET", "/")
        self.assertEqual(c.getresponse().status, 403)

    def test_server_test(self):
        url = f"http://127.0.0.1:{self.jf.server_address[1]}"
        status, j = self.post("/api/server_test", {"url": url})
        self.assertEqual(j, {"ok": True, "result": {"name": "Home", "version": "10.11.0"}})
        self.assertFalse(self.post("/api/server_test", {"url": url + "/nothing-here"})[1]["ok"])
        self.assertEqual(self.post("/api/server_test", {"url": "file:///etc/passwd"})[1]["error"], "invalid_value")

    def test_update_ops_are_allowed_for_the_phone(self):
        self.assertTrue({"update_check", "update_install"} <= server.SETTINGS_OPS)
        self.assertNotIn("pair_reset", server.SETTINGS_OPS)


if __name__ == "__main__":
    unittest.main(verbosity=2)
