#!/bin/sh
# T4 P0 probe: run the app under cage the way the kiosk unit does, at a given
# mode, inject remote keys, screenshot, optionally hand off to mpv.
# Usage (on the Pi): sudo probe-cage.sh <WxH@Hz> <label> [video]
# Artifacts: /home/pifin/probe/<label>.{log,inj.log,png,mpv.txt}
set -u
MODE=${1:-1920x1080@60Hz}; LABEL=${2:-probe}; VIDEO=${3:-}
D=/home/pifin/probe; mkdir -p $D; chown pifin:pifin $D
BIN=/home/pifin/pifin-app/target/release/pifin
SCRIPTS=/home/pifin/pifin-app/scripts
restore() { systemctl stop pifin-probe 2>/dev/null; systemctl start jellyfin-kiosk; }
trap restore EXIT

systemctl stop jellyfin-kiosk; sleep 2

# keyboard must exist before the app starts (libinput enumerates at startup)
# 100 focus moves: 25 right, 25 down/up mix, from t=8s, 0.2s apart; then Enter (mpv) and Escape
SCHED=$(python3 -c "
k=['right']*20+['down','down','left','up']*10+['left']*20
t=8.0; out=[]
for x in k: out.append(f'{t:.1f}:{x}'); t+=0.2
print(','.join(out))")
TOTAL=40
if [ -n "$VIDEO" ]; then SCHED="$SCHED,32:enter"; TOTAL=95; fi
SCHED="$SCHED,$((TOTAL-3)):esc"
python3 $SCRIPTS/inject.py --duration $TOTAL --schedule "$SCHED" > $D/$LABEL.inj.log 2>&1 &
INJ=$!
sleep 1

# the session script runs inside cage: set the mode, then exec the app
cat > $D/session.sh <<EOF
#!/bin/sh
wlr-randr --output HDMI-A-1 --mode $MODE >/dev/null 2>&1 || wlr-randr --output HDMI-A-2 --mode $MODE >/dev/null 2>&1
sleep 1
( sleep 7; grim $D/$LABEL.png; sleep 20; grim $D/$LABEL-moved.png ) &
[ -n "$VIDEO" ] && ( sleep 50; grim $D/$LABEL-mpv.png; sleep 25; grim $D/$LABEL-after.png ) &
${PROBE_ENV:-} SLINT_SCALE_FACTOR=1.5 PIFIN_FRAMELOG=1 PIFIN_EXIT=1 PIFIN_PROBE_VIDEO="$VIDEO" exec $BIN
EOF
chmod +x $D/session.sh; chown pifin:pifin $D/session.sh

systemd-run --unit=pifin-probe --collect -p User=pifin -p Group=pifin \
  -p SupplementaryGroups="input video render audio tty" -p PAMName=login \
  -p TTYPath=/dev/tty1 -p StandardInput=tty-force -p Environment=XDG_SESSION_TYPE=wayland \
  /bin/sh -c "exec /usr/bin/cage -s -- $D/session.sh > $D/$LABEL.log 2>&1"

# while mpv runs, sample its drop counter over 30 s
if [ -n "$VIDEO" ]; then
  sleep 45
  python3 - "$D/$LABEL.mpv.txt" <<'PY'
import json, socket, sys, time
def get(p):
    s = socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect("/tmp/pifin-mpv.sock")
    s.sendall((json.dumps({"command": ["get_property", p]}) + "\n").encode()); f = s.makefile()
    while True:
        m = json.loads(f.readline())
        if "event" not in m: s.close(); return m.get("data")
out = open(sys.argv[1], "w")
try:
    d0 = get("frame-drop-count"); time.sleep(30); d1 = get("frame-drop-count")
    print(f"hwdec={get('hwdec-current')} drops_30s={d1 - d0} pos={get('time-pos'):.1f}", file=out)
    s = socket.socket(socket.AF_UNIX); s.connect("/tmp/pifin-mpv.sock"); s.sendall(b'{"command":["quit"]}\n'); s.close()
except Exception as e:
    print(f"mpv probe failed: {e}", file=out)
PY
fi
wait $INJ
sleep 3
chown -R pifin:pifin $D
echo "done: $D/$LABEL.*"
