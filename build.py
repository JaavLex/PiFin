#!/usr/bin/env python3
"""Write PiFin first-boot setup to a freshly flashed Raspberry Pi OS Lite (64-bit) card.

Target: Raspberry Pi 4 (2 GB+). The first boot installs the PiFin app, mpv and comitup.

Usage:
  build.py <bootfs-mountpoint>   write the first-boot setup onto the card
  build.py --hash-password       print a PIFIN_PASS_HASH=... line for pifin.env

Configuration comes from pifin.env next to this file (copy pifin.env.example to make
one); the older file name secrets.env is still read, so existing set-ups keep working.
Required: PIFIN_PASS_HASH (or PIFIN_PASS), HOTSPOT_PASS, JELLYFIN_URL.
Everything else has a default — see pifin.env.example.
"""
import argparse
import getpass
import pathlib
import re
import subprocess
import sys

import yaml

HERE = pathlib.Path(__file__).parent
FILES = HERE / "files"

# (source file in files/, destination on the Pi, mode)
INSTALL = [
    ("pifin-setup.sh", "/usr/local/sbin/pifin-setup.sh", "0755"),
    ("pifin-setup.service", "/etc/systemd/system/pifin-setup.service", "0644"),
    ("pifin-cec-remote.py", "/usr/local/bin/pifin-cec-remote.py", "0755"),
    ("pifin-cec-remote.service", "/etc/systemd/system/pifin-cec-remote.service", "0644"),
    ("pifin-comitup-callback", "/usr/local/bin/pifin-comitup-callback", "0755"),
    ("pifin-remote/server.py", "/opt/pifin-remote/server.py", "0755"),
    ("pifin-remote/index.html", "/opt/pifin-remote/index.html", "0644"),
    ("pifin-remote/icon.svg", "/opt/pifin-remote/icon.svg", "0644"),
    ("pifin-remote/mpvplayer.py", "/opt/pifin-remote/mpvplayer.py", "0644"),
    ("pifin-remote/mpv-input.conf", "/opt/pifin-remote/mpv-input.conf", "0644"),
    ("pifin-remote/pifin-osd.lua", "/opt/pifin-remote/pifin-osd.lua", "0644"),
    ("pifin-remote.service", "/etc/systemd/system/pifin-remote.service", "0644"),
    ("pifin-app-session", "/usr/local/bin/pifin-app-session", "0755"),
    ("pifin-app.service", "/etc/systemd/system/pifin-app.service", "0644"),
    ("pifin-kiosk.pam", "/etc/pam.d/pifin-kiosk", "0644"),
    ("pifin-wifi-watchdog", "/usr/local/bin/pifin-wifi-watchdog", "0755"),
    ("pifin-wifi-watchdog.service", "/etc/systemd/system/pifin-wifi-watchdog.service", "0644"),
    ("pifin-firewall", "/usr/local/sbin/pifin-firewall", "0755"),
    ("pifin-firewall.service", "/etc/systemd/system/pifin-firewall.service", "0644"),
    ("pifin-sshd.conf", "/etc/ssh/sshd_config.d/01-pifin.conf", "0644"),
    ("99-pifin-uinput.rules", "/etc/udev/rules.d/99-pifin-uinput.rules", "0644"),
]
# Config the Pi needs at run time -> /etc/pifin/secrets.env (0600, sourced by root only)
DEVICE_KEYS = ["PIFIN_HOSTNAME", "PIFIN_USER", "WIFI_COUNTRY", "HOTSPOT_SSID",
               "HOTSPOT_PASS", "JELLYFIN_URL", "WIFI_SSID", "WIFI_PSK", "TS_AUTHKEY"]
DEFAULTS = {"PIFIN_HOSTNAME": "pifin", "PIFIN_USER": "pifin", "HOTSPOT_SSID": "PiFin-setup",
            "TIMEZONE": "Etc/UTC", "LOCALE": "en_US.UTF-8", "MAX_HEIGHT": "1080"}
# host and user names end up in YAML, shell, systemd and Tailscale: keep them plain
NAME_RE = re.compile(r"^[a-z][a-z0-9-]{0,62}$")
HASH_PREFIXES = ("$6$", "$y$", "$2a$", "$2b$")
KEY_ALGOS = ("ssh-ed25519", "ssh-rsa", "ssh-dss", "ecdsa-sha2-", "sk-", "ssh-xmss")


def load_env(path):
    """Parse a KEY=value file: skip comments, allow `export `, strip one layer of quotes."""
    env = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[len("export "):].lstrip()
        key, sep, value = line.partition("=")
        if not sep:
            sys.exit(f"{path.name}: line '{key}' has no '='")
        value = value.strip()
        if len(value) > 1 and value[0] == value[-1] and value[0] in "'\"":
            value = value[1:-1]
        env[key.strip()] = value
    return env


def sh_quote(value):
    """Single-quote for the shell's `source`. Only PIFIN_USER (plain name) is read through
    systemd's EnvironmentFile, so systemd's slightly different quoting never matters."""
    return "'" + value.replace("'", "'\\''") + "'"


def pass_hash(plain):
    """SHA-512 crypt hash, so pifin.env can hold a password instead of a hash."""
    r = subprocess.run(["openssl", "passwd", "-6", "-stdin"], input=plain.encode(),
                       capture_output=True)
    if r.returncode:
        sys.exit("could not hash with openssl — install it, or set PIFIN_PASS_HASH yourself")
    return r.stdout.decode().strip()


def load_config(explicit):
    paths = [pathlib.Path(explicit)] if explicit else [HERE / "pifin.env", HERE / "secrets.env"]
    cfg, found = {}, []
    for p in paths:
        if p.is_file():
            cfg.update(load_env(p))
            found.append(p.name)
    if not found:
        sys.exit("no config found (looked for: " + ", ".join(str(p) for p in paths)
                 + "). Copy pifin.env.example to pifin.env and fill it in.")
    for key, value in DEFAULTS.items():
        if not cfg.get(key):
            cfg[key] = value
    cfg["PIFIN_HASH"] = cfg.get("PIFIN_PASS_HASH") or (pass_hash(cfg["PIFIN_PASS"])
                                                      if cfg.get("PIFIN_PASS") else "")
    missing = [name for name, v in (("PIFIN_PASS_HASH (or PIFIN_PASS)", cfg["PIFIN_HASH"]),
                                   ("HOTSPOT_PASS", cfg.get("HOTSPOT_PASS")),
                                   ("JELLYFIN_URL", cfg.get("JELLYFIN_URL"))) if not v]
    if missing:
        sys.exit(f"{'/'.join(found)} is missing: {', '.join(missing)}")
    if not cfg["PIFIN_HASH"].startswith(HASH_PREFIXES):
        sys.exit("PIFIN_PASS_HASH is not a crypt hash — run build.py --hash-password, "
                 "or set PIFIN_PASS and let build.py hash it")
    for key in ("PIFIN_HOSTNAME", "PIFIN_USER"):
        if not NAME_RE.match(cfg[key]):
            sys.exit(f"{key} must be a lower-case name: letters, digits, '-', max 63 chars")
    if cfg.get("WIFI_PSK") and not cfg.get("WIFI_SSID"):
        sys.exit("WIFI_PSK is set but WIFI_SSID is not")
    if not cfg["MAX_HEIGHT"].isdigit():
        sys.exit("MAX_HEIGHT must be a number of pixels, e.g. 720")
    print("config from " + " + ".join(found))
    return cfg


def ssh_keys(cfg):
    """Public keys to install. Empty SSH_PUBKEY means password-only SSH (still enabled)."""
    raw = cfg.get("SSH_PUBKEY")
    if not raw:
        return []
    path = pathlib.Path(raw).expanduser()
    if not path.is_file():
        sys.exit(f"SSH_PUBKEY={path} is not a readable file — fix the path or leave it empty")
    keys = [ln.strip() for ln in path.read_text().splitlines()
            if ln.strip() and not ln.startswith("#")]
    if not keys or not all(k.startswith(KEY_ALGOS) and " " in k for k in keys):
        sys.exit(f"{path} is not an OpenSSH public key file")
    return keys


def render(text, cfg):
    """Substitute @@TOKEN@@ in installed files. Scripts read /etc/pifin/secrets.env instead;
    only the systemd units need a value baked in at build time."""
    return (text.replace("@@PIFIN_USER@@", cfg["PIFIN_USER"])
                .replace("@@PIFIN_HOSTNAME@@", cfg["PIFIN_HOSTNAME"]))


def main(args):
    boot = pathlib.Path(args.boot)
    if not boot.is_dir():
        sys.exit(f"{boot} is not a directory — mount the card's boot partition first")
    cfg = load_config(args.env)
    keys = ssh_keys(cfg)

    device_env = "".join(f"{k}={sh_quote(cfg[k])}\n" for k in DEVICE_KEYS if cfg.get(k))
    write_files = [
        {"path": "/etc/pifin/secrets.env", "permissions": "0600", "owner": "root:root",
         "content": device_env},
        # read by pifin-jellyfin-session as the kiosk user, so world readable and no secrets
        {"path": "/etc/pifin/display.conf", "permissions": "0644", "owner": "root:root",
         "content": f"MAX_HEIGHT={cfg['MAX_HEIGHT']}\n"},
        # read by the app (as the kiosk user) to find the server for Quick Connect sign-in
        {"path": "/etc/pifin/app.conf", "permissions": "0644", "owner": "root:root",
         "content": f"JELLYFIN_URL={cfg['JELLYFIN_URL']}\n"},
    ]
    write_files += [{"path": dst, "permissions": mode,
                     "content": render((FILES / src).read_text(), cfg)}
                    for src, dst, mode in INSTALL]

    user_data = {
        "hostname": cfg["PIFIN_HOSTNAME"],
        "manage_etc_hosts": True,
        "timezone": cfg["TIMEZONE"],
        "locale": cfg["LOCALE"],
        "ssh_pwauth": True,
        "users": [{
            "name": cfg["PIFIN_USER"],
            "gecos": "PiFin",
            "groups": "adm,sudo,video,render,input,audio,netdev,plugdev,users",
            "shell": "/bin/bash",
            "sudo": "ALL=(ALL) NOPASSWD:ALL",
            "lock_passwd": False,
            "passwd": cfg["PIFIN_HASH"],
            "ssh_authorized_keys": keys,
        }],
        "bootcmd": ["rfkill unblock wifi || true"],
        "write_files": write_files,
        "runcmd": [
            ["systemctl", "enable", "--now", "ssh"],
            ["systemctl", "daemon-reload"],
            ["systemctl", "enable", "pifin-setup.service"],
            ["systemctl", "start", "--no-block", "pifin-setup.service"],
        ],
    }

    # No home Wi-Fi is fine: comitup offers the setup hotspot and the TV shows a QR code.
    wlan = {"dhcp4": True, "optional": True}
    if cfg.get("WIFI_COUNTRY"):
        wlan["regulatory-domain"] = cfg["WIFI_COUNTRY"]
    if cfg.get("WIFI_SSID"):
        wlan["access-points"] = {cfg["WIFI_SSID"]:
                                 ({"password": cfg["WIFI_PSK"]} if cfg.get("WIFI_PSK") else {})}
    network = {"network": {"version": 2,
                           "ethernets": {"eth0": {"dhcp4": True, "optional": True}},
                           "wifis": {"wlan0": wlan}}}

    (boot / "user-data").write_text(
        "#cloud-config\n" + yaml.safe_dump(user_data, sort_keys=False, width=1000))
    (boot / "network-config").write_text(yaml.safe_dump(network, sort_keys=False))

    # config.txt: keep HDMI on even if the TV is off at boot; HDMI-only audio
    cfg_txt = boot / "config.txt"
    text = cfg_txt.read_text()
    marker = "# --- PiFin ---"
    if marker in text:
        text = text[: text.index(marker)].rstrip() + "\n"
    # sound over HDMI only: the 3.5mm jack would otherwise be the default output
    text = text.replace("dtparam=audio=on", "dtparam=audio=off")
    cfg_txt.write_text(text + "\n" + "\n".join([marker, "[all]", "hdmi_force_hotplug=1"]) + "\n")

    print(f"  host={cfg['PIFIN_HOSTNAME']} user={cfg['PIFIN_USER']} tz={cfg['TIMEZONE']} "
          f"max_height={cfg['MAX_HEIGHT']} ssh_keys={len(keys)}\n"
          f"  tailscale={'yes' if cfg.get('TS_AUTHKEY') else 'skipped'}  "
          f"wifi={'preconfigured' if cfg.get('WIFI_SSID') else 'setup hotspot only'}")
    print("wrote", boot / "user-data", boot / "network-config", cfg_txt)


def ask_password():
    """Interactive prompt, or one line from stdin when piped (for scripts and tests)."""
    if not sys.stdin.isatty():
        return sys.stdin.readline().strip()
    first = getpass.getpass("PiFin user password: ")
    if first != getpass.getpass("repeat it: "):
        sys.exit("passwords did not match")
    return first


def cli():
    ap = argparse.ArgumentParser(prog="build.py", description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("boot", nargs="?", help="mountpoint of the card's boot partition")
    ap.add_argument("--env", help="config file to read (default: ./pifin.env, then ./secrets.env)")
    ap.add_argument("--hash-password", action="store_true",
                    help="read a password and print a PIFIN_PASS_HASH line for pifin.env")
    args = ap.parse_args()
    if args.hash_password:
        pw = ask_password()
        if len(pw) < 8:
            sys.exit("use at least 8 characters")
        print(f"PIFIN_PASS_HASH={pass_hash(pw)}")
        return
    if not args.boot:
        ap.error("give the boot partition mountpoint, or use --hash-password")
    main(args)


if __name__ == "__main__":
    cli()
