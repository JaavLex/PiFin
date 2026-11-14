#!/usr/bin/env python3
"""Tests for files/pifin-portal (hotel login pages). No Pi, no root: paths go to a
temp dir, nmcli/systemctl/nft to fake runners.
Run: python3 tools/test_portal.py"""
import importlib.machinery
import importlib.util
import json
import os
import pathlib
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
HERE = pathlib.Path(__file__).resolve().parent.parent
# PIFIN_PORTAL_SRC: run the tests against a mutated copy (mutation checks)
_loader = importlib.machinery.SourceFileLoader(
    "portal", os.environ.get("PIFIN_PORTAL_SRC", str(HERE / "files/pifin-portal")))
_spec = importlib.util.spec_from_loader("portal", _loader)
pt = importlib.util.module_from_spec(_spec)
_loader.exec_module(pt)


def fake_sysnet(root, usb=True, builtin=True):
    """/sys/class/net with wlan0 (built-in, on mmc) and wlan1 (USB)."""
    net = root / "net"
    dev = root / "devices"
    for name, present, bus in (("wlan0", builtin, "platform/mmc1/mmc1:0001"),
                               ("wlan1", usb, "platform/usb1/1-1/1-1:1.0"),
                               ("eth0", True, "platform/eth")):
        if not present:
            continue
        (net / name).mkdir(parents=True)
        (dev / bus).mkdir(parents=True, exist_ok=True)
        os.symlink(dev / bus, net / name / "device")
        if name.startswith("wlan"):
            (net / name / "wireless").mkdir()
        (net / name / "address").write_text({"wlan0": "dc:a6:32:00:00:01\n",
                                             "wlan1": "00:c0:ca:00:00:02\n"}.get(name, "x\n"))
    return str(net)


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.cmds, self.nfts = [], []
        self.saved = {k: getattr(pt, k) for k in
                      ("sh", "nft", "SYS_NET", "WIFI_JSON", "SECRETS", "FWD_SAVE",
                       "PASS_FILE", "IP_FORWARD")}
        pt.sh = lambda *a, check=True: self.cmds.append(a)
        pt.nft = lambda text: self.nfts.append(text)
        pt.WIFI_JSON = str(self.tmp / "run/wifi.json")
        pt.SECRETS = str(self.tmp / "secrets.env")
        pt.FWD_SAVE = str(self.tmp / "run/fwd")
        pt.PASS_FILE = str(self.tmp / "run/pass")
        pt.IP_FORWARD = str(self.tmp / "ip_forward")
        (self.tmp / "ip_forward").write_text("0\n")
        (self.tmp / "secrets.env").write_text("HOTSPOT_SSID=PiFin-setup\nHOTSPOT_PASS='hot spot $ pass'\n")
        os.makedirs(self.tmp / "run")
        pt.write_json({"state": "CONNECTED", "ssid": "Hotel", "ap_ssid": "PiFin-setup",
                       "ap_pass": "x", "failed": "", "time": 1})

    def tearDown(self):
        for k, v in self.saved.items():
            setattr(pt, k, v)

    def wifi(self):
        return json.loads(pathlib.Path(pt.WIFI_JSON).read_text())

    def run_portal(self, **net):
        pt.SYS_NET = fake_sysnet(self.tmp, **net)
        slept = []
        pt.cmd_run(sleep=lambda s: (slept.append(s), self.snapshot.append(self.wifi())),
                   pause=lambda: None)
        return slept

    snapshot = None


class Decisions(Base):
    def test_events(self):
        self.assertEqual(pt.event_action("PORTAL"), "start")
        self.assertEqual(pt.event_action("FULL"), "stop")
        self.assertEqual(pt.event_action("NONE"), "stop")
        self.assertIsNone(pt.event_action("LIMITED"))
        self.assertIsNone(pt.event_action("UNKNOWN"))

    def test_event_drives_the_unit(self):
        pt.cmd_event("PORTAL")
        pt.cmd_event("FULL")
        pt.cmd_event("LIMITED")
        self.assertEqual(self.cmds, [("systemctl", "--no-block", "start", pt.UNIT),
                                     ("systemctl", "--no-block", "stop", pt.UNIT)])

    def test_plan(self):
        self.assertEqual(pt.plan("wlan1", "wlan0"), "hotspot")
        self.assertEqual(pt.plan("", "wlan0"), "mac")
        self.assertEqual(pt.plan("wlan1", ""), "mac")

    def test_radios_by_bus(self):
        self.assertEqual(pt.radios(fake_sysnet(self.tmp)), ("wlan1", "wlan0"))

    def test_nft_only_hotspot_to_link(self):
        text = pt.nft_rules("wlan0", "wlan1")
        fwd = [l for l in text.splitlines() if "portal_fwd" in l and l.startswith("add")]
        self.assertEqual(fwd, [
            'add rule inet pifin portal_fwd iifname "wlan0" oifname "wlan1" accept',
            'add rule inet pifin portal_fwd iifname "wlan1" oifname "wlan0" ct state established,related accept'])
        self.assertNotIn("tailscale0", text)
        # flushed first, so a second run never doubles the rules
        self.assertTrue(text.startswith("flush chain inet pifin portal_in\nflush chain inet pifin portal_fwd\n"))


class Flow(Base):
    def setUp(self):
        super().setUp()
        self.snapshot = []

    def test_portal_with_adapter_opens_hotspot_then_times_out(self):
        slept = self.run_portal()
        self.assertEqual(slept, [pt.TIMEOUT])
        during = self.snapshot[0]
        self.assertEqual(during["state"], "PORTAL")
        self.assertTrue(during["hotspot"])
        self.assertEqual((during["portal_ssid"], during["portal_pass"]), ("PiFin-login", "hot spot $ pass"))
        self.assertEqual(during["ssid"], "Hotel")  # comitup's fields stay
        names = [c[:4] for c in self.cmds]
        self.assertIn(("nmcli", "device", "set", "wlan0"), names)
        add = next(c for c in self.cmds if c[:3] == ("nmcli", "connection", "add"))
        self.assertIn("wlan0", add)
        self.assertNotIn("hot spot $ pass", " ".join(add))  # never on the command line
        self.assertIn(("nmcli", "--wait", "30", "connection", "up", "pifin-login", "passwd-file", pt.PASS_FILE),
                      self.cmds)
        self.assertFalse(os.path.exists(pt.PASS_FILE))
        self.assertEqual(self.nfts[0], pt.nft_rules("wlan0", "wlan1"))
        # timeout: everything torn down, the TV keeps the MAC screen
        self.assertEqual(self.nfts[-1], pt.NFT_CLEAR)
        self.assertEqual(self.cmds[-1], (pt.WIFI_SELECT,))
        after = self.wifi()
        self.assertEqual((after["state"], after["hotspot"], after["expired"]), ("PORTAL", False, True))
        self.assertEqual(after["mac"], "00:c0:ca:00:00:02")

    def test_portal_without_adapter_shows_the_mac(self):
        slept = self.run_portal(usb=False)
        self.assertEqual(slept, [])
        self.assertEqual(self.cmds, [])
        self.assertEqual(self.nfts, [])
        w = self.wifi()
        self.assertEqual((w["state"], w["hotspot"], w["mac"], w["portal_pass"]),
                         ("PORTAL", False, "dc:a6:32:00:00:01", ""))

    def test_hotspot_failure_falls_back_to_mac(self):
        import subprocess
        def boom(*a, check=True):
            self.cmds.append(a)
            if a[:4] == ("nmcli", "--wait", "30", "connection"):
                raise subprocess.CalledProcessError(4, a, stderr="no AP")
        pt.sh = boom
        slept = self.run_portal()
        self.assertEqual(slept, [])
        self.assertFalse(self.wifi()["hotspot"])
        self.assertEqual(self.nfts[-1], pt.NFT_CLEAR)
        self.assertIn((pt.WIFI_SELECT,), self.cmds)

    def test_down_cleans_everything_and_restores_connected(self):
        pt.SYS_NET = fake_sysnet(self.tmp)
        pt.write_json(pt.portal_status(self.wifi(), "hotspot", "m", "pw"))
        pathlib.Path(pt.FWD_SAVE).write_text("0\n")
        pathlib.Path(pt.IP_FORWARD).write_text("1\n")
        pt.cmd_down()
        self.assertEqual(self.nfts, [pt.NFT_CLEAR])
        self.assertIn(("nmcli", "connection", "delete", "pifin-login"), self.cmds)
        self.assertEqual(self.cmds[-1], (pt.WIFI_SELECT,))
        self.assertEqual(pathlib.Path(pt.IP_FORWARD).read_text().strip(), "0")
        w = self.wifi()
        self.assertEqual(w["state"], "CONNECTED")
        self.assertEqual(w["ssid"], "Hotel")
        for k in pt.PORTAL_KEYS:
            self.assertNotIn(k, w)

    def test_down_leaves_a_newer_comitup_state(self):
        pt.SYS_NET = fake_sysnet(self.tmp, usb=False)
        pt.write_json({"state": "HOTSPOT", "time": 5})
        pt.cmd_down()
        self.assertEqual(self.wifi(), {"state": "HOTSPOT", "time": 5})
        self.assertNotIn((pt.WIFI_SELECT,), self.cmds)  # no adapter: the built-in radio is the link


if __name__ == "__main__":
    unittest.main(verbosity=1)
