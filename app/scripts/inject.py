#!/usr/bin/env python3
"""T3 GATE helper: virtual keyboard via /dev/uinput (stdlib only; run as root).

The device is created at start and held open — Slint's libinput may only
enumerate input devices at backend init, so the keyboard MUST exist before the
spike app launches. (Same uinput mechanism the phone remote uses; libinput
classifies it as a keyboard because KEY_ESC is in the ESC..MAX range.)

Schedules (times are seconds from injector start):
  inject.py --duration 240 --every 15 left right up down   # cycle keys every 15 s
  inject.py --duration 40 a q                              # 1 tap every 2 s from t=2
  inject.py --duration 60 --schedule 8:right,20:o,30:q     # explicit t:key list
Prints one line per tap (with wall timestamps for log correlation).
"""
import argparse
import fcntl
import os
import struct
import sys
import time

EV_SYN, EV_KEY = 0, 1
SYN_REPORT = 0
UI_SET_EVBIT, UI_SET_KEYBIT = 0x40045564, 0x40045565  # _IOW('U',100/101,int)
UI_DEV_CREATE, UI_DEV_DESTROY = 0x5501, 0x5502        # _IO('U',1/2)
# NOTE: Linux input codes (input-event-codes.h), NOT xkb keycodes:
# KEY_Q=16 W=17 E=18 R=19 T=20 Y=21 U=22 I=23 O=24 P=25 (an earlier version used
# 18 for "o" → the app received "e", which is how the letter path got blamed).
KEYS = {
    "left": 105, "right": 106, "up": 103, "down": 108,
    "enter": 28, "space": 57, "o": 24, "q": 16, "esc": 1,
}


def stamp():
    return time.strftime("%H:%M:%S")


def write_ev(fd, typ, code, val):
    # struct input_event {struct timeval(2×i64), __u16 type, __u16 code, __s32 value}
    # Zero timeval is accepted by the input core (it substitutes the real time).
    os.write(fd, struct.pack("qqHHi", 0, 0, typ, code, val))


class Kbd:
    def __init__(self, name="pifin-spike-injector"):
        self.fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        fcntl.ioctl(self.fd, UI_SET_EVBIT, EV_KEY)
        for code in set(KEYS.values()):
            fcntl.ioctl(self.fd, UI_SET_KEYBIT, code)
        # struct uinput_user_dev {char name[80]; struct input_id id; s32 ff_effects_max;
        #                          s32 absmax/absmin/absfuzz/absflat[64]} = 1116 B
        pkt = (name.encode()[:79].ljust(80, b"\0")
               + struct.pack("HHHHi", 0x19, 0, 0, 0, 0)  # BUS_USB + ff_effects_max
               + b"\0" * (4 * 64 * 4))
        os.write(self.fd, pkt)
        fcntl.ioctl(self.fd, UI_DEV_CREATE)
        time.sleep(0.5)  # let udev/libinput see the device before the app starts

    def tap(self, key):
        code = KEYS[key]
        write_ev(self.fd, EV_KEY, code, 1)
        write_ev(self.fd, EV_SYN, SYN_REPORT, 0)
        time.sleep(0.06)
        write_ev(self.fd, EV_KEY, code, 0)
        write_ev(self.fd, EV_SYN, SYN_REPORT, 0)

    def close(self):
        try:
            fcntl.ioctl(self.fd, UI_DEV_DESTROY)
            os.close(self.fd)
        except OSError:
            pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=float, default=30,
                    help="hold the device open this long (s)")
    ap.add_argument("--every", type=float, default=0,
                    help="cycle the given keys every N seconds")
    ap.add_argument("--schedule", default="",
                    help="explicit taps: comma list of t:key (e.g. 8:right,20:q)")
    ap.add_argument("keys", nargs="*", choices=[*KEYS, ""])
    a = ap.parse_args()

    if not (a.keys or a.schedule):
        ap.error("no keys given")

    events = []
    if a.schedule:
        for s in a.schedule.split(","):
            t, k = s.split(":")
            events.append((float(t), k))
    elif a.every:
        t, i = a.every, 0
        while t < a.duration:
            events.append((t, a.keys[i % len(a.keys)]))
            i += 1
            t += a.every
    else:
        events = [(2.0 + 2.0 * i, k) for i, k in enumerate(a.keys)]

    kbd = Kbd()
    t0 = time.monotonic()
    print(f"{stamp()} injector: {len(events)} taps, duration={a.duration}s", flush=True)
    for t, k in sorted(events):
        rem = t0 + t - time.monotonic()
        if rem > 0:
            time.sleep(rem)
        kbd.tap(k)
        print(f"{stamp()} tap {k} @ {time.monotonic() - t0:.1f}s", flush=True)
    rem = t0 + a.duration - time.monotonic()
    if rem > 0:
        time.sleep(rem)
    kbd.close()
    print(f"{stamp()} injector: done", flush=True)


if __name__ == "__main__":
    main()
