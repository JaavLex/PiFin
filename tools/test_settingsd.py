#!/usr/bin/env python3
"""Tests for files/pifin-settingsd. No Pi, no root: every path is under a temp
PIFIN_ROOT and every system command goes to a fake runner.
Run: python3 tools/test_settingsd.py"""
import glob
import importlib.machinery
import importlib.util
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

sys.dont_write_bytecode = True
HERE = pathlib.Path(__file__).resolve().parent.parent
_loader = importlib.machinery.SourceFileLoader("settingsd", str(HERE / "files/pifin-settingsd"))
_spec = importlib.util.spec_from_loader("settingsd", _loader)
sd = importlib.util.module_from_spec(_spec)
_loader.exec_module(sd)

USER = "testuser"
OLD_PASS = "old hot $pass"
AUTH = {"server": "http://old.example:8096", "user_id": "u1", "token": "tok1", "device_id": "d1",
        "users": [{"user_id": "u1", "name": "Ann", "token": "tok1", "device_id": "d1"}]}


class Base(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="sd")
        self.addCleanup(shutil.rmtree, self.root, True)
        os.environ["PIFIN_ROOT"] = self.root
        self.write("/etc/pifin/app.conf", "JELLYFIN_URL=http://jellyfin.example:8096\n")
        self.write("/etc/pifin/display.conf", "MAX_HEIGHT=720\n")
        # old-format secrets.env: no HOTSPOT_SSID line
        self.write("/etc/pifin/secrets.env",
                   f"PIFIN_USER='{USER}'\nWIFI_COUNTRY='DE'\nHOTSPOT_PASS='{OLD_PASS}'\n"
                   "JELLYFIN_URL='http://other.example:8096'\nTS_AUTHKEY='tskey-test-abc'\n")
        self.write("/etc/comitup.conf",
                   f"ap_name: Test-Hotspot\nap_password: '{OLD_PASS}'\nweb_service:\n"
                   "enable_appliance_mode: false\n"
                   "external_callback: /usr/local/bin/pifin-comitup-callback\n"
                   "primary_wifi_device: wlan1\n")
        self.write(f"/home/{USER}/.config/pifin/auth.json", json.dumps(AUTH))
        self.write("/run/pifin/wifi.json", json.dumps({"state": "CONNECTED", "ssid": "HomeNet"}))
        self.calls = []
        self.comitup_state = 'ss "CONNECTED" "HomeNet"\n'
        sd.RUN = self.fake_run
        sd.NOW = lambda: self.clock
        self.clock = 1000.0
        sd.FAILS = 0
        sd.LOCK_UNTIL = 0.0
        sd.init()

    def fake_run(self, argv, timeout=10):
        self.calls.append(list(argv))
        if argv[0] == "busctl":
            return 0, self.comitup_state
        if argv[-1] == "--version":
            return 0, "pifin 0.2.0\n"
        return 0, ""

    def path(self, rel):
        return self.root + rel

    def write(self, rel, text):
        p = self.path(rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(text)

    def read(self, rel):
        with open(self.path(rel)) as f:
            return f.read()

    def settings(self):
        return json.loads(self.read("/var/lib/pifin/settings.json"))

    def secrets(self):
        return json.loads(self.read("/var/lib/pifin/secrets.json"))

    def ok(self, req):
        r = sd.handle(req)
        self.assertTrue(r["ok"], r)
        return r["result"]

    def err(self, req, code):
        r = sd.handle(req)
        self.assertFalse(r["ok"], r)
        self.assertEqual(r["error"], code, r)
        return r


class T01Migration(Base):
    def test_migration_fills_every_field(self):
        s = self.settings()
        self.assertEqual(s, {
            "version": 1,
            "playback": {"max_bitrate": 0, "max_height": 1080, "hdr": False, "prefer_original": True},
            "networks": {},
            "display": {"output_max_height": 720, "subtitle_scale": 1.0, "audio_passthrough": False},
            "system": {"jellyfin_url": "http://jellyfin.example:8096",
                       "hotspot_ssid": "Test-Hotspot", "pin_set": False},
        })
        self.assertEqual(self.secrets(), {"pin_hash": None, "hotspot_pass": OLD_PASS})
        st = os.stat(self.path("/var/lib/pifin/secrets.json")).st_mode & 0o777
        self.assertEqual(st, 0o600)
        token = self.read("/var/lib/pifin/pair-token")
        self.assertRegex(token, r"^[0-9a-f]{32}$")
        new = self.ok({"op": "pair_reset"})["token"]
        self.assertNotEqual(new, token)
        self.assertEqual(self.read("/var/lib/pifin/pair-token"), new)
        # legacy files untouched by migration
        self.assertNotIn("HOTSPOT_SSID", self.read("/etc/pifin/secrets.env"))


class T02AllOrNothing(Base):
    def test_one_bad_key_writes_nothing(self):
        before = self.read("/var/lib/pifin/settings.json")
        r = self.err({"op": "set", "changes": {"playback.hdr": True, "playback.max_height": 999}},
                     "invalid_value")
        self.assertIn("Max resolution", r["message"])
        self.assertEqual(self.read("/var/lib/pifin/settings.json"), before)
        self.ok({"op": "set", "changes": {"playback.hdr": True, "display.subtitle_scale": 1.25}})
        self.assertTrue(self.settings()["playback"]["hdr"])
        self.assertEqual(self.settings()["display"]["subtitle_scale"], 1.25)


class T03Validation(Base):
    def test_unknown_key_and_bool_types(self):
        self.err({"op": "set", "changes": {"playback.bogus": 1}}, "bad_request")
        self.err({"op": "set", "changes": {"system.pin_set": True}}, "bad_request")
        self.err({"op": "set", "changes": {"playback.hdr": 1}}, "invalid_value")
        self.err({"op": "set", "changes": {"playback.max_bitrate": False}}, "invalid_value")
        self.err({"op": "set", "changes": {"system.jellyfin_url": "javascript:alert(1)"}},
                 "invalid_value")
        self.err({"op": "set", "changes": {"system.hotspot_ssid": "evil\nap_password: x"}},
                 "invalid_value")
        self.err({"op": "set", "changes": {"secrets.hotspot_pass": "short"}}, "invalid_value")
        for name in (" Spot", "Spot ", "'Spot", "#Spot"):  # unquoted ap_name would change
            self.err({"op": "set", "changes": {"system.hotspot_ssid": name}}, "invalid_value")
        self.err({"op": "nope"}, "bad_request")
        self.assertEqual(sd.handle_line(b"not json\n")["error"], "bad_request")


class T04Pin(Base):
    def test_set_check_change_clear(self):
        self.err({"op": "pin_set", "pin": "12a4"}, "invalid_value")
        self.ok({"op": "pin_set", "pin": "1234"})
        self.assertTrue(self.settings()["system"]["pin_set"])
        self.assertTrue(self.secrets()["pin_hash"].startswith("pbkdf2_sha256$200000$"))
        self.assertIs(self.ok({"op": "pin_check", "pin": "1234"}), True)
        self.err({"op": "pin_check", "pin": "9999"}, "pin_wrong")
        self.err({"op": "pin_set", "pin": "5678"}, "pin_required")
        self.err({"op": "pin_set", "pin": "5678", "old_pin": "0000"}, "pin_wrong")
        self.ok({"op": "pin_set", "pin": "5678", "old_pin": "1234"})
        self.err({"op": "pin_check", "pin": "1234"}, "pin_wrong")
        self.err({"op": "pin_clear", "pin": "1234"}, "pin_wrong")
        self.ok({"op": "pin_clear", "pin": "5678"})
        self.assertFalse(self.settings()["system"]["pin_set"])
        self.assertIsNone(self.secrets()["pin_hash"])


class T05Lockout(Base):
    def test_lockout_after_five_wrong(self):
        self.ok({"op": "pin_set", "pin": "1234"})
        # a correct PIN resets the counter
        for _ in range(4):
            self.err({"op": "pin_check", "pin": "0000"}, "pin_wrong")
        self.ok({"op": "pin_check", "pin": "1234"})
        for _ in range(4):
            self.err({"op": "pin_check", "pin": "0000"}, "pin_wrong")
        self.err({"op": "pin_check", "pin": "0000"}, "locked_out")
        # even the right PIN, on any op, during the lock
        self.err({"op": "pin_check", "pin": "1234"}, "locked_out")
        self.clock += 59
        self.err({"op": "set", "pin": "1234",
                  "changes": {"system.jellyfin_url": "http://a.example"}}, "locked_out")
        self.clock += 2
        self.ok({"op": "pin_check", "pin": "1234"})


class T06Sensitive(Base):
    def test_sensitive_needs_pin(self):
        self.ok({"op": "pin_set", "pin": "1234"})
        self.err({"op": "set", "changes": {"system.jellyfin_url": "http://new.example:8096"}},
                 "pin_required")
        self.err({"op": "set", "changes": {"secrets.hotspot_pass": "newpassword"}}, "pin_required")
        self.ok({"op": "set", "changes": {"playback.hdr": True}})
        self.ok({"op": "set", "pin": "1234",
                 "changes": {"system.jellyfin_url": "http://new.example:8096"}})


class T07JellyfinUrl(Base):
    def test_url_rewrites_legacy_files(self):
        self.err({"op": "set", "changes": {"system.jellyfin_url": ""}}, "invalid_value")
        url = "https://media.example:8920/jf"
        self.ok({"op": "set", "changes": {"system.jellyfin_url": url}})
        self.assertEqual(self.settings()["system"]["jellyfin_url"], url)
        self.assertEqual(self.read("/etc/pifin/app.conf"), f"JELLYFIN_URL={url}\n")
        auth = json.loads(self.read(f"/home/{USER}/.config/pifin/auth.json"))
        self.assertEqual(auth, dict(AUTH, server=url))
        mode = os.stat(self.path(f"/home/{USER}/.config/pifin/auth.json")).st_mode & 0o777
        self.assertEqual(mode, 0o600)
        self.assertIn(["systemctl", "restart", "pifin-firewall"], self.calls)


class T08Hotspot(Base):
    def source(self, var):
        return subprocess.run(["bash", "-c", f'. "$1"; printf %s "${var}"', "_",
                               self.path("/etc/pifin/secrets.env")],
                              capture_output=True, text=True).stdout

    def test_hotspot_pass_and_ssid(self):
        pw = "it's a $pass"
        inode = os.stat(self.path("/etc/comitup.conf")).st_ino
        self.ok({"op": "set", "changes": {"secrets.hotspot_pass": pw}})
        self.assertEqual(self.secrets()["hotspot_pass"], pw)
        env = self.read("/etc/pifin/secrets.env")
        self.assertIn("WIFI_COUNTRY='DE'\n", env)
        self.assertIn("TS_AUTHKEY='tskey-test-abc'\n", env)
        self.assertEqual(env.count("HOTSPOT_PASS="), 1)
        self.assertEqual(self.source("HOTSPOT_PASS"), pw)
        self.assertEqual(os.stat(self.path("/etc/pifin/secrets.env")).st_mode & 0o777, 0o600)
        conf = self.read("/etc/comitup.conf")
        self.assertIn("primary_wifi_device: wlan1\n", conf)
        # edited in place: the unit only lets the daemon write that one file in /etc
        self.assertEqual(os.stat(self.path("/etc/comitup.conf")).st_ino, inode)
        self.assertIn("ap_password: 'it''s a $pass'\n", conf)
        self.assertEqual(sd.parse_comitup(self.path("/etc/comitup.conf"))["ap_password"], pw)
        self.assertNotIn(["systemctl", "restart", "comitup"], self.calls)  # not in HOTSPOT

        self.comitup_state = 'ss "HOTSPOT" "Test-Hotspot"\n'
        self.ok({"op": "set", "changes": {"system.hotspot_ssid": "New Spot"}})
        self.assertEqual(self.source("HOTSPOT_SSID"), "New Spot")
        self.assertEqual(self.source("HOTSPOT_PASS"), pw)
        self.assertIn("ap_name: New Spot\n", self.read("/etc/comitup.conf"))
        self.assertIn(["systemctl", "restart", "comitup"], self.calls)


class T09Networks(Base):
    def test_network_limits_set_and_removed(self):
        self.ok({"op": "network_set", "ssid": "Cafe", "limits": {"max_bitrate": 6000000}})
        self.assertEqual(self.settings()["networks"], {"Cafe": {"max_bitrate": 6000000}})
        self.err({"op": "network_set", "ssid": "Cafe", "limits": {"bogus": 1}}, "bad_request")
        self.err({"op": "network_set", "ssid": "x" * 33, "limits": {"hdr": True}}, "invalid_value")
        self.ok({"op": "network_set", "ssid": "Cafe", "limits": None})
        self.assertEqual(self.settings()["networks"], {})


class T10Corrupt(Base):
    def test_corrupt_settings_renamed_and_remigrated(self):
        self.ok({"op": "pin_set", "pin": "1234"})
        self.write("/var/lib/pifin/settings.json", "{not json")
        sd.init()
        bad = glob.glob(self.path("/var/lib/pifin/settings.json.bad-*"))
        self.assertEqual(len(bad), 1)
        s = self.settings()
        sd.validate(s)
        self.assertEqual(s["system"]["hotspot_ssid"], "Test-Hotspot")
        self.assertTrue(s["system"]["pin_set"])  # the PIN survives a bad settings.json


class T11Socket(Base):
    def test_socket_round_trip(self):
        server = sd.make_server()
        t = threading.Thread(target=server.serve_forever, daemon=True)
        t.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        sock_path = self.path("/run/pifin/settings.sock")
        self.assertEqual(os.stat(sock_path).st_mode & 0o777, 0o660)
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.settimeout(3)
        c.connect(sock_path)
        f = c.makefile("rwb")
        f.write(b'{"op":"get"}\n')
        f.flush()
        resp = json.loads(f.readline())
        self.assertTrue(resp["ok"], resp)
        st = resp["result"]["status"]
        self.assertEqual(st["wifi"]["state"], "CONNECTED")
        self.assertEqual(st["wifi"]["ssid"], "HomeNet")
        self.assertEqual(st["version"], "0.2.0")
        self.assertEqual(set(st), {"wifi", "macs", "tailscale", "version"})
        f.write(b"garbage\n")
        f.flush()
        self.assertEqual(json.loads(f.readline())["error"], "bad_request")
        f.close()
        c.close()


class T12WriteAtomic(Base):
    def test_mode_and_owner_set_on_the_fd_not_the_path(self):
        target = self.path("/etc/pifin/x.conf")
        with mock.patch("os.chmod", side_effect=AssertionError("path chmod")), \
             mock.patch("os.chown", side_effect=AssertionError("path chown")):
            sd.write_atomic(target, "A=1\n", 0o640, 0, 0)
        self.assertEqual(os.stat(target).st_mode & 0o777, 0o640)
        self.assertEqual(self.read("/etc/pifin/x.conf"), "A=1\n")

    def test_symlink_at_the_temp_name_is_not_followed(self):
        victim = self.path("/victim")
        self.write("/victim", "victim\n")
        d = self.path(f"/home/{USER}/.config/pifin")
        os.symlink(victim, os.path.join(d, ".auth.json.aaaaaaaaaaaa"))
        with mock.patch.object(sd.secrets, "token_hex", return_value="aaaaaaaaaaaa"):
            with self.assertRaises(sd.Unsafe):
                sd.write_atomic(os.path.join(d, "auth.json"), "{}", 0o600, owner=os.getuid())
        self.assertEqual(self.read("/victim"), "victim\n")
        self.assertEqual(json.loads(self.read(f"/home/{USER}/.config/pifin/auth.json")), AUTH)


class T13AuthJsonSafety(Base):
    URL = {"op": "set", "changes": {"system.jellyfin_url": "http://new.example:8096"}}

    def assert_refused(self):
        before = {p: self.read(p) for p in ("/var/lib/pifin/settings.json",
                                            "/var/lib/pifin/secrets.json", "/etc/pifin/app.conf")}
        self.err(self.URL, "failed")
        for p, text in before.items():
            self.assertEqual(self.read(p), text, p)
        self.assertNotIn(["systemctl", "restart", "pifin-firewall"], self.calls)

    def test_symlinked_auth_json_is_refused(self):
        auth = self.path(f"/home/{USER}/.config/pifin/auth.json")
        os.unlink(auth)
        os.symlink(self.path("/var/lib/pifin/secrets.json"), auth)
        self.assert_refused()
        self.assertTrue(os.path.islink(auth))

    def test_auth_json_not_a_regular_file_is_refused(self):
        auth = self.path(f"/home/{USER}/.config/pifin/auth.json")
        os.unlink(auth)
        os.mkfifo(auth)
        self.assert_refused()

    def test_foreign_owner_is_refused(self):
        with mock.patch.object(sd, "user_uid", return_value=os.getuid() + 1):
            self.assert_refused()

    def test_symlinked_directory_is_refused(self):
        d = self.path(f"/home/{USER}/.config/pifin")
        os.rename(d, d + "-real")
        os.symlink(d + "-real", d)
        self.assert_refused()

    def test_migration_ignores_a_symlinked_auth_json(self):
        os.unlink(self.path("/etc/pifin/app.conf"))
        self.write("/etc/pifin/secrets.env", f"PIFIN_USER='{USER}'\n")
        self.write("/elsewhere.json", json.dumps({"server": "http://evil.example:8096"}))
        auth = self.path(f"/home/{USER}/.config/pifin/auth.json")
        os.unlink(auth)
        os.symlink(self.path("/elsewhere.json"), auth)
        os.unlink(self.path("/var/lib/pifin/settings.json"))
        sd.init()
        self.assertEqual(self.settings()["system"]["jellyfin_url"], "")


class T14MigrationFallbacks(Base):
    def remigrate(self):
        for f in ("settings.json", "secrets.json"):
            os.unlink(self.path(f"/var/lib/pifin/{f}"))
        sd.init()
        return self.settings(), self.secrets()

    def test_second_and_third_sources(self):
        os.unlink(self.path("/etc/pifin/app.conf"))
        self.write("/etc/pifin/display.conf", "MAX_HEIGHT=480\n")
        self.write("/etc/comitup.conf", "ap_password: 'it''s comitup'\nprimary_wifi_device: wlan1\n")
        self.write("/etc/pifin/secrets.env", f"PIFIN_USER='{USER}'\nHOTSPOT_SSID='Env-Spot'\n"
                   "JELLYFIN_URL='http://other.example:8096'\n")
        s, sec = self.remigrate()
        self.assertEqual(s["system"]["jellyfin_url"], "http://other.example:8096")
        self.assertEqual(s["system"]["hotspot_ssid"], "Env-Spot")
        self.assertEqual(s["display"]["output_max_height"], 1080)
        self.assertEqual(sec["hotspot_pass"], "it's comitup")
        self.write("/etc/pifin/secrets.env", f"PIFIN_USER='{USER}'\n")
        s, _ = self.remigrate()
        self.assertEqual(s["system"]["jellyfin_url"], "http://old.example:8096")  # auth.json
        self.assertEqual(s["system"]["hotspot_ssid"], "PiFin-setup")

    def test_lost_secrets_json_keeps_the_hotspot_password(self):
        os.unlink(self.path("/var/lib/pifin/secrets.json"))
        sd.init()
        self.assertEqual(self.secrets(), {"pin_hash": None, "hotspot_pass": OLD_PASS})


class T15Status(Base):
    def test_non_string_wifi_fields_are_replaced(self):
        self.write("/run/pifin/wifi.json", json.dumps({"state": 5, "ssid": ["x"]}))
        w = self.ok({"op": "get"})["status"]["wifi"]
        self.assertEqual((w["state"], w["ssid"]), ("unknown", ""))


if __name__ == "__main__":
    unittest.main(verbosity=2)
