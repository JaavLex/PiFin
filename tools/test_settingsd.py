#!/usr/bin/env python3
"""Tests for files/pifin-settingsd. No Pi, no root: every path is under a temp
PIFIN_ROOT and every system command goes to a fake runner.
Run: python3 tools/test_settingsd.py"""
import configparser
import glob
import importlib.machinery
import importlib.util
import io
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import types
import unittest
from unittest import mock

sys.dont_write_bytecode = True
HERE = pathlib.Path(__file__).resolve().parent.parent
_loader = importlib.machinery.SourceFileLoader("settingsd", str(HERE / "files/pifin-settingsd"))
_spec = importlib.util.spec_from_loader("settingsd", _loader)
sd = importlib.util.module_from_spec(_spec)
_loader.exec_module(sd)
BOOT_REBOOT_MARK = sd.LAST_PHONE_REBOOT  # the daemon's value at start

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
                   f"ap_name: Test-Hotspot\nap_password: {OLD_PASS}\nweb_service:\n"
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
        sd.LAST_PHONE_REBOOT = BOOT_REBOOT_MARK
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


def comitup_reads(conf):
    """comitup's own parser (comitup/config.py): a plain ConfigParser, no unquoting."""
    cp = configparser.ConfigParser()
    cp.read_string("[DEFAULT]\n" + conf)
    return dict(cp["DEFAULT"])


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
        for pw in (" password", "password "):  # configparser strips edge spaces
            self.err({"op": "set", "changes": {"secrets.hotspot_pass": pw}}, "invalid_value")
        for url in ("http://a.example:99999", "http://a.example:65536", "http://a.example:0"):
            self.err({"op": "set", "changes": {"system.jellyfin_url": url}}, "invalid_value")
        self.ok({"op": "set", "changes": {"system.jellyfin_url": "http://a.example:65535"}})
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
        # another server: its saved logins are dropped, the device id stays
        self.assertEqual(auth, dict(AUTH, server=url, users=[], token="", user_id=""))
        mode = os.stat(self.path(f"/home/{USER}/.config/pifin/auth.json")).st_mode & 0o777
        self.assertEqual(mode, 0o600)
        self.assertIn(["systemctl", "restart", "pifin-firewall"], self.calls)


class T07bJellyfinLogins(Base):
    def auth(self):
        return json.loads(self.read(f"/home/{USER}/.config/pifin/auth.json"))

    def set_url(self, url):
        self.ok({"op": "set", "changes": {"system.jellyfin_url": url}})

    def test_same_origin_keeps_the_logins(self):
        for url in ("http://old.example:8096/", "http://Old.Example:8096/jellyfin",
                    "http://old.example:8096"):
            self.set_url(url)
            self.assertEqual(self.auth(), dict(AUTH, server=url), url)

    def test_default_port_counts_as_the_same_origin(self):
        self.write(f"/home/{USER}/.config/pifin/auth.json",
                   json.dumps(dict(AUTH, server="https://old.example")))
        self.set_url("https://old.example:443/jf")
        self.assertEqual(self.auth()["users"], AUTH["users"])

    def test_other_host_port_or_scheme_drops_the_logins(self):
        for url in ("http://evil.example:8096", "http://old.example:8097",
                    "https://old.example:8096"):
            self.write(f"/home/{USER}/.config/pifin/auth.json", json.dumps(AUTH))
            self.set_url(url)
            a = self.auth()
            self.assertEqual((a["users"], a["token"], a["user_id"]), ([], "", ""), url)
            self.assertEqual((a["server"], a["device_id"]), (url, "d1"), url)
            self.assertNotIn("tok1", self.read(f"/home/{USER}/.config/pifin/auth.json"), url)

    def test_missing_old_server_drops_the_logins(self):
        self.write(f"/home/{USER}/.config/pifin/auth.json",
                   json.dumps({k: v for k, v in AUTH.items() if k != "server"}))
        self.set_url("http://old.example:8096")
        self.assertEqual(self.auth()["users"], [])


class T08Hotspot(Base):
    def source(self, var):
        return subprocess.run(["bash", "-c", f'. "$1"; printf %s "${var}"', "_",
                               self.path("/etc/pifin/secrets.env")],
                              capture_output=True, text=True).stdout

    def test_hotspot_pass_and_ssid(self):
        pw = "it's 100% a $pass"
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
        self.assertIn("ap_password: it's 100%% a $pass\n", conf)
        self.assertEqual(comitup_reads(conf)["ap_password"], pw)  # what comitup itself sees
        self.assertEqual(sd.parse_comitup(self.path("/etc/comitup.conf"))["ap_password"], pw)
        self.assertNotIn(["systemctl", "restart", "comitup"], self.calls)  # not in HOTSPOT

        self.comitup_state = 'ss "HOTSPOT" "Test-Hotspot"\n'
        self.ok({"op": "set", "changes": {"system.hotspot_ssid": "New 50% Spot"}})
        self.assertEqual(self.source("HOTSPOT_SSID"), "New 50% Spot")
        self.assertEqual(self.source("HOTSPOT_PASS"), pw)
        conf = self.read("/etc/comitup.conf")
        self.assertIn("ap_name: New 50%% Spot\n", conf)
        self.assertEqual(comitup_reads(conf)["ap_name"], "New 50% Spot")
        self.assertEqual(comitup_reads(conf)["ap_password"], pw)
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
        self.write("/etc/comitup.conf", "ap_password: it's 100%% comitup\nprimary_wifi_device: wlan1\n")
        self.write("/etc/pifin/secrets.env", f"PIFIN_USER='{USER}'\nHOTSPOT_SSID='Env-Spot'\n"
                   "JELLYFIN_URL='http://other.example:8096'\n")
        s, sec = self.remigrate()
        self.assertEqual(s["system"]["jellyfin_url"], "http://other.example:8096")
        self.assertEqual(s["system"]["hotspot_ssid"], "Env-Spot")
        self.assertEqual(s["display"]["output_max_height"], 1080)
        self.assertEqual(sec["hotspot_pass"], "it's 100% comitup")
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


FIX = HERE / "tools/fixtures/settingsd"
HOME_UUID = "00000000-0000-4000-8000-000000000001"
OFFICE_UUID = "00000000-0000-4000-8000-000000000006"
PSK = "s3cret wifi pass"
KEY = "tskey-auth-kAbCdEf123456-XyZ"
SCAN = ("nmcli", "-t", "-f", "SSID,SIGNAL,SECURITY", "dev", "wifi", "list", "--rescan", "yes")


class FixtureBase(Base):
    """The daemon's own demo runner on the captured Pi outputs, every call recorded."""

    def setUp(self):
        super().setUp()
        saved = {n: getattr(sd, n) for n in ("COMITUP_CONNECT", "SPAWN", "SLEEP", "TAILSCALE_PRESENT")}
        self.addCleanup(lambda: [setattr(sd, n, v) for n, v in saved.items()])
        self.fixture = sd.fixture_runner(str(FIX))
        self.overrides = {}  # argv prefix tuple -> (rc, out)
        self.calls = []
        sd.RUN = self.fix_run
        self.joins = []
        self.connects = []
        sd.COMITUP_CONNECT = lambda ssid, psk: self.connects.append((ssid, psk))
        sd.SPAWN = lambda fn: self.joins.append(sd.spawn(fn))
        sd.SLEEP = lambda s: None

    def fix_run(self, argv, timeout=10):
        self.calls.append(list(argv))
        for prefix, answer in self.overrides.items():
            if tuple(argv[:len(prefix)]) == prefix:
                return answer
        return self.fixture(argv, timeout)

    def join(self):
        for t in self.joins:
            t.join(5)
            self.assertFalse(t.is_alive())

    def set_pin(self, pin="1234"):
        self.ok({"op": "pin_set", "pin": pin})

    def install_tailscale(self):
        os.makedirs(self.path("/usr/bin"), exist_ok=True)
        open(self.path("/usr/bin/tailscale"), "w").close()


class T16WifiList(FixtureBase):
    def test_hotspot_profiles_are_dropped(self):
        nets = self.ok({"op": "wifi_list"})
        self.assertEqual(nets, [{"ssid": "Example-Home", "uuid": HOME_UUID, "priority": 0},
                                {"ssid": "Example-Office", "uuid": OFFICE_UUID, "priority": 0}])


class T17WifiScan(FixtureBase):
    def test_dedupe_sort_saved_secured(self):
        out = self.fixture(list(SCAN))[1] + "Example-Cafe:31:--\n"
        self.overrides[SCAN] = (0, out)
        nets = self.ok({"op": "wifi_scan"})
        self.assertEqual(nets, [
            {"ssid": "Example-Home", "signal": 58, "secured": True, "saved": True},
            {"ssid": "Example-Library", "signal": 45, "secured": True, "saved": False},
            {"ssid": "Example-Cafe", "signal": 31, "secured": False, "saved": False},
        ])

    def test_rescan_failure_retries_without_rescan(self):
        self.overrides[SCAN] = (1, "")
        nets = self.ok({"op": "wifi_scan"})
        self.assertEqual([n["ssid"] for n in nets], ["Example-Home", "Example-Library"])
        self.assertEqual(self.calls[-1][-2:], ["--rescan", "no"])

    def test_hotspot_state_uses_comitup_list(self):
        self.write("/run/pifin/wifi.json", json.dumps({"state": "HOTSPOT", "ssid": ""}))
        aps = [{"ssid": "Example-Office", "strength": "40", "security": "encrypted"},
               {"ssid": "Example-Cafe", "strength": "71", "security": "unencrypted"}]
        self.overrides[("busctl", "--json=short")] = (0, json.dumps({"type": "aa{ss}", "data": [aps]}))
        nets = self.ok({"op": "wifi_scan"})
        self.assertEqual(nets, [
            {"ssid": "Example-Cafe", "signal": 71, "secured": False, "saved": False},
            {"ssid": "Example-Office", "signal": 40, "secured": True, "saved": True},
        ])
        self.assertFalse(any("wifi" in c and "list" in c for c in self.calls))


class T18Unescape(FixtureBase):
    def test_escaped_colon_in_ssid(self):
        self.overrides[("nmcli", "-g", "802-11-wireless.mode,802-11-wireless.ssid", "con", "show",
                        HOME_UUID)] = (0, "infrastructure\nMy\\:Net\n")
        self.overrides[SCAN] = (0, "My\\:Net:60:WPA2\nBack\\\\slash:20:\n")
        self.assertEqual(self.ok({"op": "wifi_list"})[0]["ssid"], "My:Net")
        nets = self.ok({"op": "wifi_scan"})
        self.assertEqual([(n["ssid"], n["saved"]) for n in nets],
                         [("My:Net", True), ("Back\\slash", False)])


class T19WifiAdd(FixtureBase):
    def test_psk_goes_to_comitup_never_to_argv(self):
        self.assertEqual(self.ok({"op": "wifi_add", "ssid": "Example-Cafe", "psk": PSK}),
                         {"connecting": True})
        self.assertEqual(self.connects, [("Example-Cafe", PSK)])
        self.assertFalse(any(PSK in " ".join(c) for c in self.calls), self.calls)
        self.ok({"op": "wifi_add", "ssid": "Example-Open", "psk": ""})
        self.assertEqual(self.connects[-1], ("Example-Open", ""))

    def test_bad_psk_refused_before_comitup(self):
        self.err({"op": "wifi_add", "ssid": "Example-Cafe", "psk": "7chars!"}, "invalid_value")
        self.err({"op": "wifi_add", "ssid": "", "psk": PSK}, "invalid_value")
        self.assertEqual(self.connects, [])

    def test_without_dbus_module_fails_cleanly(self):
        sd.COMITUP_CONNECT = sd._dbus_connect
        err = io.StringIO()
        with mock.patch.dict(sys.modules, {"dbus": None}), mock.patch.object(sys, "stderr", err):
            r = self.err({"op": "wifi_add", "ssid": "Example-Cafe", "psk": PSK}, "failed")
        self.assertEqual(r["message"], "Couldn't start joining the network.")
        self.assertNotIn(PSK, err.getvalue())

    def test_dbus_call_shape_with_a_fake_module(self):
        seen = []
        fake = types.ModuleType("dbus")
        fake.DBusException = type("DBusException", (Exception,), {})

        class Iface:
            def __init__(self, obj, name):
                seen.append(("iface", obj, name))

            def connect(self, ssid, psk, timeout):
                seen.append(("connect", ssid, psk, timeout))
                if ssid == "Example-Bad":
                    raise fake.DBusException(f"no {ssid} {psk}")

        fake.Interface = Iface
        fake.SystemBus = lambda: types.SimpleNamespace(get_object=lambda n, p: ("obj", n, p))
        sd.COMITUP_CONNECT = sd._dbus_connect
        err = io.StringIO()
        with mock.patch.dict(sys.modules, {"dbus": fake}), mock.patch.object(sys, "stderr", err):
            self.ok({"op": "wifi_add", "ssid": "Example-Cafe", "psk": PSK})
            r = self.err({"op": "wifi_add", "ssid": "Example-Bad", "psk": PSK}, "failed")
        bus = ("obj", "com.github.davesteele.comitup", "/com/github/davesteele/comitup")
        self.assertEqual(seen[:2], [("iface", bus, "com.github.davesteele.comitup"),
                                    ("connect", "Example-Cafe", PSK, 10)])
        self.assertNotIn(PSK, err.getvalue() + json.dumps(r))
        self.assertEqual(r["message"], sd.JOIN_FAILED)


class T20WifiForget(FixtureBase):
    def test_delete_by_uuid_and_drop_limits(self):
        self.ok({"op": "network_set", "ssid": "Example-Office", "limits": {"max_height": 720}})
        self.ok({"op": "network_set", "ssid": "Example-Home", "limits": {"hdr": True}})
        s = self.ok({"op": "wifi_forget", "ssid": "Example-Office"})["settings"]
        self.assertIn(["nmcli", "con", "delete", "uuid", OFFICE_UUID], self.calls)
        self.assertEqual(sum(1 for c in self.calls if "delete" in c), 1)
        self.assertEqual(s["networks"], {"Example-Home": {"hdr": True}})
        self.assertEqual(self.settings()["networks"], {"Example-Home": {"hdr": True}})
        self.err({"op": "wifi_forget", "ssid": "Example-Library"}, "invalid_value")


class T20bPhoneRules(FixtureBase):
    """Requests the phone-remote server forwards carry via=phone; the TV's do not."""

    def test_wifi_add_and_forget_need_the_pin_from_a_phone(self):
        self.set_pin()
        add = {"op": "wifi_add", "ssid": "Example-Cafe", "psk": PSK, "via": "phone"}
        self.err(add, "pin_required")
        self.err(dict(add, pin="0000"), "pin_wrong")
        self.assertEqual(self.connects, [])
        self.ok(dict(add, pin="1234"))
        self.assertEqual(self.connects, [("Example-Cafe", PSK)])
        forget = {"op": "wifi_forget", "ssid": "Example-Office", "via": "phone"}
        self.err(forget, "pin_required")
        self.assertFalse(any("delete" in c for c in self.calls))
        self.ok(dict(forget, pin="1234"))
        self.assertIn(["nmcli", "con", "delete", "uuid", OFFICE_UUID], self.calls)

    def test_wifi_from_the_tv_or_without_a_pin_is_free(self):
        self.ok({"op": "wifi_add", "ssid": "Example-Cafe", "psk": PSK, "via": "phone"})
        self.set_pin()
        self.ok({"op": "wifi_add", "ssid": "Example-Cafe", "psk": PSK})
        self.ok({"op": "wifi_forget", "ssid": "Example-Office"})

    def test_first_pin_only_from_the_tv(self):
        r = self.err({"op": "pin_set", "pin": "1234", "via": "phone"}, "tv_only")
        self.assertEqual(r["message"], "Set the first PIN on the TV.")
        self.assertFalse(self.settings()["system"]["pin_set"])
        self.set_pin()
        # changing it from the phone still works with the old PIN
        self.err({"op": "pin_set", "pin": "5678", "via": "phone"}, "pin_required")
        self.ok({"op": "pin_set", "pin": "5678", "old_pin": "1234", "via": "phone"})
        self.ok({"op": "pin_check", "pin": "5678"})

    def test_phone_reboot_at_most_every_five_minutes(self):
        self.ok({"op": "reboot", "via": "phone"})
        self.clock += 299
        r = self.err({"op": "reboot", "via": "phone"}, "too_soon")
        self.assertIn("Try again", r["message"])
        self.assertEqual(len(self.joins), 1)
        self.ok({"op": "reboot"})  # the TV is never limited
        self.clock += 1
        self.ok({"op": "reboot", "via": "phone"})
        self.join()
        self.assertEqual(len(self.joins), 3)

    def test_no_phone_reboot_in_the_first_minutes_after_boot(self):
        self.clock = 120.0  # the monotonic clock counts from boot
        self.err({"op": "reboot", "via": "phone"}, "too_soon")
        self.ok({"op": "reboot"})
        self.clock = 300.0
        self.ok({"op": "reboot", "via": "phone"})


class T21TailscaleLogin(FixtureBase):
    def test_pin_gate_and_key_never_logged(self):
        self.install_tailscale()
        self.set_pin()
        self.calls.clear()
        self.err({"op": "tailscale_login", "authkey": KEY}, "pin_required")
        self.assertEqual(self.calls, [])
        err = io.StringIO()
        with mock.patch.object(sys, "stderr", err):
            self.assertIsNone(self.ok({"op": "tailscale_login", "authkey": KEY, "pin": "1234"}))
            up = [c for c in self.calls if c[:2] == ["tailscale", "up"]]
            self.assertEqual(len(up), 1)
            self.assertIn("--authkey=" + KEY, up[0])  # the accepted argv exposure
            self.overrides[("tailscale", "up")] = (1, "")
            r = self.err({"op": "tailscale_login", "authkey": KEY, "pin": "1234"}, "failed")
            self.assertEqual(r["message"], "Tailscale sign-in failed. Check the key.")
        self.assertIn("tailscale up failed", err.getvalue())
        self.assertNotIn(KEY, err.getvalue())
        self.assertNotIn(KEY, json.dumps(r))
        self.err({"op": "tailscale_login", "authkey": "tskey-short", "pin": "1234"}, "invalid_value")

    def test_not_installed(self):
        r = self.err({"op": "tailscale_login", "authkey": KEY}, "failed")
        self.assertEqual(r["message"], "Tailscale isn't installed on this PiFin.")


class T22Status(FixtureBase):
    def test_status_from_fixtures(self):
        net = self.path("/sys/class/net")
        devs = self.path("/sys/devices/platform")
        layout = {"wlan1": (f"{devs}/scb/usb1/1-1/1-1.3/1-1.3:1.0", "02:00:00:00:00:02", True),
                  "wlan0": (f"{devs}/soc/mmc1/mmc1:0001/mmc1:0001:1", "02:00:00:00:00:01", True),
                  "eth0": (f"{devs}/scb/eth0", "02:00:00:00:00:03", False)}
        for name, (dev, mac, wireless) in layout.items():
            os.makedirs(dev)
            os.makedirs(f"{net}/{name}")
            os.symlink(dev, f"{net}/{name}/device")
            self.write(f"/sys/class/net/{name}/address", mac + "\n")
            if wireless:
                os.makedirs(f"{net}/{name}/wireless")
        os.makedirs(self.path("/proc/net"))
        shutil.copy(FIX / "proc-net-wireless.txt", self.path("/proc/net/wireless"))
        self.install_tailscale()
        st = self.ok({"op": "get"})["status"]
        self.assertEqual(st["wifi"], {"state": "CONNECTED", "ssid": "HomeNet", "radio": "usb",
                                      "iface": "wlan1", "signal_dbm": -68, "ip": "192.0.2.10"})
        self.assertEqual(st["macs"], {"builtin": "02:00:00:00:00:01", "usb": "02:00:00:00:00:02"})
        self.assertEqual(st["tailscale"], {"state": "connected", "name": "pifin", "ip": "192.0.2.100"})

    def test_missing_sources_give_null(self):
        st = self.ok({"op": "get"})["status"]  # no /sys, no /proc, no tailscale binary
        self.assertEqual((st["wifi"]["radio"], st["wifi"]["signal_dbm"]), (None, None))
        self.assertEqual(st["macs"], {"builtin": None, "usb": None})
        self.assertEqual(st["tailscale"], {"state": "not_installed", "name": None, "ip": None})


class T23Reboot(FixtureBase):
    def test_reboot_after_reply(self):
        self.assertEqual(sd.handle({"op": "reboot"}), {"ok": True, "result": None})
        self.join()
        self.assertEqual(self.calls[-1], ["systemctl", "reboot"])

    def test_factory_reset(self):
        self.set_pin()
        self.err({"op": "factory_reset"}, "pin_required")
        self.assertEqual(self.joins, [])
        self.calls.clear()
        sd.SLEEP = lambda s: self.calls.append(["sleep", s])
        self.assertIsNone(self.ok({"op": "factory_reset", "pin": "1234"}))
        self.join()
        self.assertEqual(self.calls[0], ["sleep", 1])  # the reply goes out before the reset
        for f in ("settings.json", "secrets.json", "pair-token"):
            self.assertFalse(os.path.exists(self.path(f"/var/lib/pifin/{f}")), f)
        self.assertFalse(os.path.exists(self.path(f"/home/{USER}/.config/pifin/auth.json")))
        deleted = {c[4] for c in self.calls if c[:4] == ["nmcli", "con", "delete", "uuid"]}
        self.assertEqual(deleted, {HOME_UUID, OFFICE_UUID})
        self.assertEqual(self.calls[-1], ["systemctl", "reboot"])

    def test_factory_reset_does_not_follow_a_symlinked_dir(self):
        d = self.path(f"/home/{USER}/.config/pifin")
        os.rename(d, d + "-real")
        os.symlink(d + "-real", d)
        self.ok({"op": "factory_reset"})
        self.join()
        self.assertTrue(os.path.exists(d + "-real/auth.json"))
        self.assertEqual(self.calls[-1], ["systemctl", "reboot"])


class T25IdlePoll(unittest.TestCase):
    def test_daemon_does_not_wake_when_idle(self):
        server = mock.Mock()
        with mock.patch.object(sd, "init"), mock.patch.object(sd, "make_server", return_value=server), \
                mock.patch.object(sd, "log"), mock.patch.object(sys, "argv", ["pifin-settingsd"]):
            old = os.umask(0o022)
            try:
                sd.main()
            finally:
                os.umask(old)
        self.assertGreaterEqual(server.serve_forever.call_args.kwargs.get("poll_interval", 0.5), 600)


class T24Demo(unittest.TestCase):
    def test_demo_daemon_answers_from_fixtures(self):
        root = tempfile.mkdtemp(prefix="sd")
        self.addCleanup(shutil.rmtree, root, True)
        env = dict(os.environ, PIFIN_ROOT=root, PATH="/nonexistent")  # no real command can run
        p = subprocess.Popen([sys.executable, str(HERE / "files/pifin-settingsd"), "--demo", str(FIX)],
                             env=env, stderr=subprocess.DEVNULL)
        self.addCleanup(p.wait, 5)
        self.addCleanup(p.terminate)
        sock_path = root + "/run/pifin/settings.sock"
        for _ in range(100):
            if os.path.exists(sock_path):
                break
            threading.Event().wait(0.05)
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.settimeout(5)
        c.connect(sock_path)
        self.addCleanup(c.close)
        f = c.makefile("rwb")
        self.addCleanup(f.close)

        def ask(op):
            f.write(json.dumps({"op": op}).encode() + b"\n")
            f.flush()
            r = json.loads(f.readline())
            self.assertTrue(r["ok"], r)
            return r["result"]

        st = ask("get")["status"]
        self.assertEqual(st["tailscale"]["state"], "connected")
        self.assertEqual((st["wifi"]["iface"], st["wifi"]["ip"]), ("wlan1", "192.0.2.10"))
        self.assertEqual([n["ssid"] for n in ask("wifi_list")], ["Example-Home", "Example-Office"])
        self.assertEqual(ask("wifi_scan")[0], {"ssid": "Example-Home", "signal": 58,
                                               "secured": True, "saved": True})

    def test_demo_refuses_without_pifin_root(self):
        env = {k: v for k, v in os.environ.items() if k != "PIFIN_ROOT"}
        r = subprocess.run([sys.executable, str(HERE / "files/pifin-settingsd"), "--demo", str(FIX)],
                           env=env, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("PIFIN_ROOT", r.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
