#!/usr/bin/env bash
# Build PiFin into a throwaway "card" with made-up values and check the result.
# No real Pi and no real credentials: everything here is fake. Safe to run anywhere.
set -uo pipefail
cd "$(dirname "$0")/.."
fails=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; fails=$((fails + 1)); }

D=$(mktemp -d); trap 'rm -rf "$D"' EXIT
mkdir -p "$D/bootfs" "$D/legacy"
printf 'dtparam=audio=on\ndtoverlay=vc4-kms-v3d\n[cm4]\ndtparam=foo=bar\n' > "$D/bootfs/config.txt"
ssh-keygen -q -t ed25519 -N '' -C selftest -f "$D/id_ed25519"

echo "== syntax =="
python3 -m py_compile build.py files/*.py files/pifin-remote/*.py \
  && ok "python compiles" || bad "python compiles"
for f in files/pifin-setup.sh files/pifin-comitup-callback; do
  bash -n "$f" && ok "bash -n $(basename "$f")" || bad "bash -n $(basename "$f")"
done
if command -v node >/dev/null; then
  node --check tools/shot-remote.mjs && ok "node --check shot-remote.mjs" || bad "node --check shot-remote.mjs"
fi

echo "== A: full pifin.env =="
# the hotspot password carries a space, a $ and a quote on purpose: it must survive
# build.py -> YAML -> bash source -> comitup.conf
cat > "$D/pifin.env" <<EOF
PIFIN_PASS=selftest-password
JELLYFIN_URL=http://jellyfin.example:8096
HOTSPOT_PASS=hot spot \$ 'pass'
TS_AUTHKEY=tskey-selftest-abc
WIFI_SSID=Example-Cafe
WIFI_PSK=psk with space
WIFI_COUNTRY=DE
TIMEZONE=Etc/UTC
PIFIN_HOSTNAME=testhub
PIFIN_USER=testuser
SSH_PUBKEY=$D/id_ed25519.pub
MAX_HEIGHT=1080
EOF
python3 build.py --env "$D/pifin.env" "$D/bootfs" >/dev/null && ok "build.py ran" || bad "build.py failed"
cat > "$D/check_a.py" <<'PY'
import os, pathlib, shlex, subprocess, sys
import yaml
D = pathlib.Path(os.environ["D"])
bad = 0
def eq(label, got, want):
    global bad
    if got != want:
        bad += 1
        print(f"  FAIL {label}: got {got!r} want {want!r}")
u = yaml.safe_load((D / "bootfs/user-data").read_text())
n = yaml.safe_load((D / "bootfs/network-config").read_text())
eq("hostname", u["hostname"], "testhub")
eq("timezone", u["timezone"], "Etc/UTC")
user = u["users"][0]
eq("user name", user["name"], "testuser")
eq("password hashed", user["passwd"][:3], "$6$")
eq("ssh key installed", user["ssh_authorized_keys"], [os.environ["WANT"]])
wf = {f["path"]: f for f in u["write_files"]}
eq("file count", len(wf), 16)  # 14 installed + secrets.env + display.conf
eq("display.conf", wf["/etc/pifin/display.conf"]["content"], "MAX_HEIGHT=1080\n")
eq("display.conf mode", wf["/etc/pifin/display.conf"]["permissions"], "0644")
dev = wf["/etc/pifin/secrets.env"]["content"]
env = {}
for line in dev.splitlines():
    k, _, v = line.partition("=")
    env[k] = shlex.split(v)[0]
eq("hotspot pass survives quoting", env["HOTSPOT_PASS"], "hot spot $ 'pass'")
eq("wifi psk survives quoting", env["WIFI_PSK"], "psk with space")
eq("device user", env["PIFIN_USER"], "testuser")
eq("device hostname", env["PIFIN_HOSTNAME"], "testhub")
eq("tailscale key passed through", "TS_AUTHKEY" in env, True)
eq("no plaintext password on the card", "PIFIN_PASS" in dev, False)
for f in u["write_files"]:
    if "@@" in f["content"]:
        bad += 1
        print(f"  FAIL unsubstituted token in {f['path']}")
kiosk = wf["/etc/systemd/system/jellyfin-kiosk.service"]["content"]
eq("kiosk runs as the configured user", "User=testuser\n" in kiosk, True)
wlan = n["network"]["wifis"]["wlan0"]
eq("regulatory domain", wlan["regulatory-domain"], "DE")
eq("home wifi", wlan["access-points"], {"Example-Cafe": {"password": "psk with space"}})
(D / "device.env").write_text(dev)
got = subprocess.run(["bash", "-c", '. "$1"; printf %s "$HOTSPOT_PASS"', "_", str(D / "device.env")],
                     capture_output=True, text=True).stdout
eq("device env survives 'source'", got, "hot spot $ 'pass'")
txt = (D / "bootfs/config.txt").read_text()
eq("HDMI hotplug appended", txt.count("# --- PiFin ---"), 1)
eq("audio moved to HDMI", "dtparam=audio=off" in txt, True)
eq("other config.txt lines kept", "[cm4]\ndtparam=foo=bar" in txt, True)
sys.exit(1 if bad else 0)
PY
if D="$D" WANT="$(cat "$D/id_ed25519.pub")" python3 "$D/check_a.py"; then
  ok "user-data + network-config + device env"
else
  bad "user-data + network-config + device env"
fi

echo "== B: nothing optional set =="
mkdir -p "$D/bare"
printf 'dtparam=audio=on\n' > "$D/bare/config.txt"
hash=$(python3 -c "import build; print(build.pass_hash('selftest-password'))")
printf 'PIFIN_PASS_HASH=%s\nJELLYFIN_URL=http://192.0.2.10:8096\nHOTSPOT_PASS=selftest-hotspot\n' \
  "$hash" > "$D/min.env"
python3 build.py --env "$D/min.env" "$D/bare" >/dev/null && ok "build.py ran on a minimal config" || bad "build.py failed on a minimal config"
cat > "$D/check_b.py" <<'PY'
import os, pathlib, shlex, sys
import yaml
D = pathlib.Path(os.environ["D"])
bad = 0
def eq(label, got, want):
    global bad
    if got != want:
        bad += 1
        print(f"  FAIL {label}: got {got!r} want {want!r}")
u = yaml.safe_load((D / "bare/user-data").read_text())
n = yaml.safe_load((D / "bare/network-config").read_text())
wf = {f["path"]: f for f in u["write_files"]}
eq("default hostname", u["hostname"], "pifin")
eq("default user", u["users"][0]["name"], "pifin")
eq("no ssh keys", u["users"][0]["ssh_authorized_keys"], [])
wlan = n["network"]["wifis"]["wlan0"]
eq("no wifi preconfigured", "access-points" in wlan, False)
eq("no regulatory domain", "regulatory-domain" in wlan, False)
eq("eth0 still there", n["network"]["ethernets"]["eth0"]["dhcp4"], True)
env = {l.partition("=")[0]: l for l in wf["/etc/pifin/secrets.env"]["content"].splitlines()}
eq("no tailscale line at all", "TS_AUTHKEY=" in "".join(env.values()), False)
eq("hotspot ssid default", shlex.split(env["HOTSPOT_SSID"].partition("=")[2])[0], "PiFin-setup")
eq("display.conf default", wf["/etc/pifin/display.conf"]["content"], "MAX_HEIGHT=720\n")
sys.exit(1 if bad else 0)
PY
if D="$D" python3 "$D/check_b.py"; then ok "defaults, no wifi/tailscale/ssh"; else bad "defaults, no wifi/tailscale/ssh"; fi

echo "== C: the older secrets.env still works =="
mkdir -p "$D/proj" && cp build.py "$D/proj/build.py" && ln -s "$PWD/files" "$D/proj/files"
printf 'PIFIN_PASS_HASH=%s\nTS_AUTHKEY=tskey-old-abc\nHOTSPOT_PASS=old-hotspot\nWIFI_SSID=OldNet\nWIFI_PSK=old-psk\nJELLYFIN_URL=http://jellyfin.old:8096\n' \
  "$hash" > "$D/proj/secrets.env"
mkdir -p "$D/oldcard" && printf 'dtparam=audio=on\n' > "$D/oldcard/config.txt"
if (cd "$D/proj" && python3 build.py "$D/oldcard" >/dev/null); then
  if python3 -c "
import yaml,sys
u=yaml.safe_load(open('$D/oldcard/user-data'))
wf={f['path']:f for f in u['write_files']}
d=wf['/etc/pifin/secrets.env']['content']
sys.exit(0 if u['hostname']=='pifin' and 'tskey-old-abc' in d and 'MAX_HEIGHT=720' in wf['/etc/pifin/display.conf']['content'] else 1)"
  then ok "legacy secrets.env + defaults"; else bad "legacy secrets.env result wrong"; fi
else bad "build.py failed with only a secrets.env"; fi

echo "== D: bad configs are refused =="
for case in 'no password: JELLYFIN_URL=http://x\nHOTSPOT_PASS=y' \
            'plaintext in the hash field: PIFIN_PASS_HASH=hunter2hunter2\nJELLYFIN_URL=http://x\nHOTSPOT_PASS=y' \
            'bad name: PIFIN_PASS=pwlongenough\nJELLYFIN_URL=http://x\nHOTSPOT_PASS=y\nPIFIN_USER=Test User' \
            'psk without ssid: PIFIN_PASS=pwlongenough\nJELLYFIN_URL=http://x\nHOTSPOT_PASS=y\nWIFI_PSK=abc' \
            'missing key file: PIFIN_PASS=pwlongenough\nJELLYFIN_URL=http://x\nHOTSPOT_PASS=y\nSSH_PUBKEY=/nonexistent/nope.pub'; do
  label="${case%%:*}"; body="${case#*: }"
  printf "$body\n" > "$D/bad.env"
  if python3 build.py --env "$D/bad.env" "$D/bare" >/dev/null 2>&1; then
    bad "refused to reject: $label"
  else
    ok "rejected: $label"
  fi
done

echo
if [ "$fails" -eq 0 ]; then echo "selftest passed"; else echo "selftest: $fails failure(s)"; exit 1; fi
