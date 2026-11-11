#!/usr/bin/env python3
"""Bridge the TV remote (HDMI-CEC) to keyboard events for Jellyfin Desktop.

Runs cec-client in monitor mode, parses "key pressed: <name>" lines and emits
the matching key on a virtual (uinput) keyboard.
"""
import re
import subprocess
import sys
import time

from evdev import UInput, ecodes as e

KEYMAP = {
    "up": e.KEY_UP,
    "down": e.KEY_DOWN,
    "left": e.KEY_LEFT,
    "right": e.KEY_RIGHT,
    "select": e.KEY_ENTER,
    "enter": e.KEY_ENTER,
    "exit": e.KEY_ESC,
    "back": e.KEY_ESC,
    "return": e.KEY_ESC,
    "root menu": e.KEY_HOMEPAGE,
    "setup menu": e.KEY_HOMEPAGE,
    "contents menu": e.KEY_MENU,
    "play": e.KEY_PLAYPAUSE,
    "pause": e.KEY_PLAYPAUSE,
    "stop": e.KEY_STOPCD,
    "fast forward": e.KEY_FASTFORWARD,
    "rewind": e.KEY_REWIND,
    "forward": e.KEY_NEXTSONG,
    "backward": e.KEY_PREVIOUSSONG,
    "channel up": e.KEY_PAGEUP,
    "channel down": e.KEY_PAGEDOWN,
    "F1 (blue)": e.KEY_F1,
    "F2 (red)": e.KEY_F2,
    "F3 (green)": e.KEY_F3,
    "F4 (yellow)": e.KEY_F4,
}
for d in range(10):
    KEYMAP[str(d)] = getattr(e, f"KEY_{d}")

PRESSED = re.compile(r"key pressed: (.+?) \(")

ui = UInput({e.EV_KEY: sorted(set(KEYMAP.values()))}, name="pifin-cec-remote")


def tap(code):
    ui.write(e.EV_KEY, code, 1)
    ui.syn()
    ui.write(e.EV_KEY, code, 0)
    ui.syn()


while True:
    # -t p: playback device, -o: OSD name shown on the TV, -d 16: debug log includes "key pressed"
    proc = subprocess.Popen(
        ["cec-client", "-t", "p", "-o", "Jellyfin", "-d", "16"],
        stdout=subprocess.PIPE, stdin=subprocess.PIPE, text=True, bufsize=1,
    )
    # Announce ourselves as the active source: many TVs (Samsung Anynet+,
    # LG SimpLink...) only forward remote buttons to the active source.
    time.sleep(5)
    try:
        proc.stdin.write("as\n")
        proc.stdin.flush()
    except BrokenPipeError:
        pass
    for line in proc.stdout:
        m = PRESSED.search(line)
        if m and m.group(1) in KEYMAP:
            print("CEC key:", m.group(1), flush=True)
            tap(KEYMAP[m.group(1)])
        elif m:
            print("unmapped CEC key:", m.group(1), file=sys.stderr, flush=True)
    proc.wait()
    time.sleep(3)  # cec-client exited (TV off/unplugged): restart
