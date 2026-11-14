#!/usr/bin/env python3
"""Tests for "Check for updates": files/pifin-update (fake GitHub, fake root) and the
daemon's update_check / update_install ops. No network, no root.
Run: python3 tools/test_updates.py"""
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
import pathlib
import shutil
import sys
import tarfile
import tempfile
import unittest

sys.dont_write_bytecode = True
HERE = pathlib.Path(__file__).resolve().parent.parent


def load(name, path):
    loader = importlib.machinery.SourceFileLoader(name, str(HERE / path))
    mod = importlib.util.module_from_spec(importlib.util.spec_from_loader(name, loader))
    loader.exec_module(mod)
    return mod


up = load("pifin_update", "files/pifin-update")
sd = load("settingsd_u", "files/pifin-settingsd")


def sha(b):
    return hashlib.sha256(b).hexdigest()


def files_tarball(extra=None):
    """A pifin-files.tar.gz with a two-entry install.list."""
    entries = {
        "files/install.list": b"# comment\nhello.service /etc/systemd/system/hello.service 0644\n"
                              b"pifin-sshd.conf /etc/ssh/sshd_config.d/01-pifin.conf 0644\n",
        "files/hello.service": b"[Service]\nUser=@@PIFIN_USER@@\n",
        "files/pifin-sshd.conf": b"new sshd\n",
    }
    entries.update(extra or {})
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        for name, data in entries.items():
            ti = tarfile.TarInfo(name)
            ti.size = len(data)
            tar.addfile(ti, io.BytesIO(data))
    return buf.getvalue()


class FakeGitHub:
    def __init__(self, tag="v0.3.0", app=b"\x7fELF new app", files=None, digests=True, sums=True):
        self.files = files if files is not None else files_tarball()
        self.blobs = {"pifin-aarch64": app, "pifin-files.tar.gz": self.files}
        assets = []
        for name, data in self.blobs.items():
            a = {"name": name, "size": len(data), "browser_download_url": f"https://example.invalid/{name}"}
            if digests:
                a["digest"] = "sha256:" + sha(data)
            assets.append(a)
        if sums:
            self.blobs["SHA256SUMS"] = "".join(f"{sha(d)}  {n}\n" for n, d in list(self.blobs.items())).encode()
            assets.append({"name": "SHA256SUMS", "browser_download_url": "https://example.invalid/SHA256SUMS"})
        self.release = {"tag_name": tag, "draft": False, "prerelease": False, "body": "notes", "assets": assets}
        self.urls = []

    def __call__(self, url, limit=up.MAX_DOWNLOAD):
        self.urls.append(url)
        if url == up.API:
            return json.dumps(self.release).encode()
        return self.blobs[url.rsplit("/", 1)[1]]


class Root(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="upd")
        self.addCleanup(shutil.rmtree, self.root, True)
        os.environ["PIFIN_ROOT"] = self.root
        self.write("/etc/pifin/version", "0.2.0\n")
        self.write("/usr/local/bin/pifin", "old app")
        self.write("/etc/systemd/system/pifin-app.service", "[Service]\nUser=kiosk\n")
        self.write("/etc/systemd/system/hello.service", "old hello\n")
        self.write("/etc/ssh/sshd_config.d/01-pifin.conf", "PasswordAuthentication no\n")
        self.calls = []
        self.mpv = False
        up.RUN = self.fake_run

    def fake_run(self, argv, timeout=60):
        self.calls.append(list(argv))
        if argv[0] == "pgrep":
            return (0, "42\n") if self.mpv else (1, "")
        return 0, ""

    def write(self, rel, text):
        p = self.root + rel
        os.makedirs(os.path.dirname(p), exist_ok=True)
        pathlib.Path(p).write_text(text)

    def read(self, rel):
        return pathlib.Path(self.root + rel).read_text()


class Versions(unittest.TestCase):
    def test_compare(self):
        self.assertTrue(up.newer("0.3.0", "0.2.0"))
        self.assertTrue(up.newer("v0.10.0", "0.9.9"))     # numeric, not text order
        self.assertTrue(up.newer("1.0", "0.9.9"))
        self.assertFalse(up.newer("0.2.0", "0.2.0"))
        self.assertFalse(up.newer("0.1.9", "0.2.0"))
        self.assertFalse(up.newer("0.3.0-rc1", "0.2.0"))  # pre-release tags are never offered
        self.assertTrue(up.newer("0.3.0", ""))            # unknown installed version: offer it


class Assets(unittest.TestCase):
    def test_picks_the_update_files(self):
        a = up.pick_assets(FakeGitHub().release)
        self.assertEqual(set(a), {"pifin-aarch64", "pifin-files.tar.gz", "SHA256SUMS"})

    def test_release_without_update_files(self):
        rel = FakeGitHub().release
        rel["assets"] = [x for x in rel["assets"] if x["name"] != "pifin-files.tar.gz"]
        with self.assertRaises(up.UpdateError):
            up.pick_assets(rel)

    def test_drafts_and_non_https_urls_are_ignored(self):
        rel = FakeGitHub().release
        with self.assertRaises(up.UpdateError):
            up.pick_assets(dict(rel, draft=True))
        rel["assets"][0]["browser_download_url"] = "http://example.invalid/pifin-aarch64"
        with self.assertRaises(up.UpdateError):
            up.pick_assets(rel)

    def test_checksums_must_agree_and_exist(self):
        h = "a" * 64
        self.assertEqual(up.expected_sha("x", {"digest": "sha256:" + h}, {"x": h}), h)
        self.assertEqual(up.expected_sha("x", {}, {"x": h}), h)
        with self.assertRaises(up.UpdateError):
            up.expected_sha("x", {"digest": "sha256:" + h}, {"x": "b" * 64})
        with self.assertRaises(up.UpdateError):
            up.expected_sha("x", {}, {})


class Check(Root):
    def test_newer_release_is_offered(self):
        up.FETCH = FakeGitHub()
        doc = up.check()
        self.assertEqual((doc["current"], doc["latest"], doc["available"]), ("0.2.0", "0.3.0", True))
        self.assertEqual(json.loads(self.read("/run/pifin/update.json"))["state"], "available")

    def test_same_version_is_up_to_date(self):
        up.FETCH = FakeGitHub(tag="v0.2.0")
        self.assertFalse(up.check()["available"])

    def test_offline_leaves_an_error_state(self):
        def offline(url, limit=0):
            raise OSError("no network")
        up.FETCH = offline
        with self.assertRaises(SystemExit):
            up.main(["pifin-update", "check"])
        self.assertEqual(json.loads(self.read("/run/pifin/update.json"))["state"], "error")


class Install(Root):
    def test_installs_files_binary_and_version(self):
        up.FETCH = FakeGitHub()
        self.assertEqual(up.install()["state"], "done")
        self.assertEqual(self.read("/usr/local/bin/pifin"), "\x7fELF new app")
        self.assertEqual(self.read("/etc/systemd/system/hello.service"), "[Service]\nUser=kiosk\n")
        self.assertEqual(self.read("/etc/pifin/version"), "0.3.0\n")
        # the per-device sshd file is kept
        self.assertEqual(self.read("/etc/ssh/sshd_config.d/01-pifin.conf"), "PasswordAuthentication no\n")
        # backups of what was replaced
        self.assertEqual(self.read("/var/lib/pifin/update-backup/0.2.0/usr/local/bin/pifin"), "old app")
        self.assertIn(["systemctl", "try-restart", "pifin-app.service"], self.calls)
        self.assertEqual(self.calls[-1], ["systemctl", "try-restart", "pifin-app.service"])

    def test_refused_while_playing(self):
        self.mpv = True
        gh = up.FETCH = FakeGitHub()
        with self.assertRaises(up.UpdateError):
            up.install()
        self.assertEqual(gh.urls, [])
        self.assertEqual(self.read("/usr/local/bin/pifin"), "old app")

    def test_damaged_download_installs_nothing(self):
        gh = FakeGitHub()
        gh.blobs["pifin-aarch64"] = b"tampered"
        up.FETCH = gh
        with self.assertRaises(up.UpdateError):
            up.install()
        self.assertEqual(self.read("/usr/local/bin/pifin"), "old app")
        self.assertEqual(self.read("/etc/pifin/version"), "0.2.0\n")

    def test_failure_midway_rolls_back(self):
        # the list names a file the tarball lacks: hello.service is put back
        bad = files_tarball({"files/install.list": b"hello.service /etc/systemd/system/hello.service 0644\n"
                                                   b"missing /usr/local/bin/missing 0755\n"})
        up.FETCH = FakeGitHub(files=bad)
        with self.assertRaises(FileNotFoundError):
            up.install()
        self.assertEqual(self.read("/etc/systemd/system/hello.service"), "old hello\n")
        self.assertEqual(self.read("/usr/local/bin/pifin"), "old app")

    def test_tarball_with_a_path_outside_files_is_refused(self):
        up.FETCH = FakeGitHub(files=files_tarball({"../evil": b"x"}))
        with self.assertRaises(up.UpdateError):
            up.install()
        self.assertFalse(os.path.exists(os.path.join(os.path.dirname(self.root), "evil")))

    def test_tarball_with_a_symlink_is_refused(self):
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as tar:
            ti = tarfile.TarInfo("files/install.list")
            ti.type, ti.linkname = tarfile.SYMTYPE, "/etc/shadow"
            tar.addfile(ti)
        up.FETCH = FakeGitHub(files=buf.getvalue())
        with self.assertRaises(up.UpdateError):
            up.install()
        self.assertEqual(self.read("/usr/local/bin/pifin"), "old app")


class DaemonOps(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="sdu")
        self.addCleanup(shutil.rmtree, self.root, True)
        os.environ["PIFIN_ROOT"] = self.root
        os.makedirs(self.root + "/run/pifin")
        self.calls, self.mpv, self.answer, self.rc = [], False, None, 0
        sd.RUN = self.fake

    def fake(self, argv, timeout=10):
        self.calls.append(list(argv))
        if argv == ["systemctl", "start", "pifin-update@check.service"]:
            if self.answer is not None:
                pathlib.Path(self.root + "/run/pifin/update.json").write_text(json.dumps(self.answer))
            return self.rc, ""
        if argv[0] == "pgrep":
            return (0, "42\n") if self.mpv else (1, "")
        return 0, ""

    def test_check_answers_the_state_file(self):
        self.answer = {"state": "available", "current": "0.2.0", "latest": "0.3.0", "available": True, "notes": "n" * 900}
        r = sd.handle({"op": "update_check"})["result"]
        self.assertEqual((r["current"], r["latest"], r["available"], len(r["notes"])), ("0.2.0", "0.3.0", True, 600))

    def test_check_never_returns_a_stale_answer(self):
        # (the check unit succeeded but wrote nothing): an error, not the old answer
        pathlib.Path(self.root + "/run/pifin/update.json").write_text(
            json.dumps({"state": "available", "latest": "9.9.9", "available": True}))
        r = sd.handle({"op": "update_check"})
        self.assertEqual(r["error"], "failed")

    def test_check_error_message_is_shown(self):
        self.answer, self.rc = {"state": "error", "message": "Couldn't reach GitHub. Is PiFin online?"}, 1
        self.assertIn("GitHub", sd.handle({"op": "update_check"})["message"])

    def test_install_refused_while_playing(self):
        self.mpv = True
        self.assertEqual(sd.handle({"op": "update_install"})["error"], "busy")
        self.assertFalse(any("pifin-update@install.service" in c for c in self.calls))

    def test_install_starts_the_unit(self):
        self.assertEqual(sd.handle({"op": "update_install"}), {"ok": True, "result": {"started": True}})
        self.assertIn(["systemctl", "start", "--no-block", "pifin-update@install.service"], self.calls)


if __name__ == "__main__":
    unittest.main(verbosity=2)
