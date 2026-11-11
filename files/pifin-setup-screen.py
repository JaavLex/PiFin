#!/usr/bin/env python3
"""Show the Wi-Fi setup instructions full screen on the TV.

Drives the Jellyfin Desktop window through its local DevTools port
(127.0.0.1:9222) and navigates it to a self-contained instructions page with a
QR code to join the setup hotspot. Getting back to Jellyfin is done by
restarting jellyfin-kiosk (see pifin-comitup-callback).

Usage:
  pifin-setup-screen.py hotspot <hotspot-ssid> <hotspot-password> [failed-ssid]
  pifin-setup-screen.py connecting <ssid>
  pifin-setup-screen.py shown        # exit 0 when the TV is on a PiFin page

Exit code 0 = the page is on the TV (or already was), non-zero = something is
wrong (DevTools down, no page target, navigation refused). The comitup callback
discards this script's output, so the exit code is the only signal it gets.
"""
import html
import io
import json
import sys
import urllib.error
import urllib.parse
import urllib.request

DEVTOOLS = "http://127.0.0.1:9222/json/list"

CSS = """
 html,body{margin:0;height:100%;background:#101018;color:#f2f2f7;font-family:sans-serif}
 .wrap{display:flex;height:100%;align-items:center;justify-content:center;gap:6vw;padding:0 6vw;box-sizing:border-box}
 h1{font-size:5vh;margin:0 0 4vh;color:#aa5cc3}
 ol{font-size:3.4vh;line-height:1.7;margin:0;padding-left:1.2em}
 code{background:#2a2a3a;padding:.1em .4em;border-radius:.3em;color:#fff}
 .qr{background:#fff;padding:2vh;border-radius:2vh;width:38vh;height:38vh}
 .qr svg{width:100%;height:100%}
 .hint{font-size:2.4vh;color:#9a9ab0;margin-top:4vh}
 .err{font-size:2.8vh;color:#ff9f9f}
 .big{font-size:6vh}
 .spin{width:8vh;height:8vh;border:1vh solid #2a2a3a;border-top-color:#aa5cc3;border-radius:50%;margin:0 auto 4vh;animation:spin 1.2s linear infinite}
 @keyframes spin{to{transform:rotate(360deg)}}
"""


def wifi_qr_svg(ssid, password):
    import qrcode
    import qrcode.image.svg

    esc = lambda s: "".join("\\" + c if c in '\\;,:"' else c for c in s)
    payload = f"WIFI:T:WPA;S:{esc(ssid)};P:{esc(password)};;"
    img = qrcode.make(payload, image_factory=qrcode.image.svg.SvgPathImage, box_size=12, border=2)
    buf = io.BytesIO()
    img.save(buf)
    return buf.getvalue().decode()


def page(ssid, password, failed=None):
    s, p = html.escape(ssid), html.escape(password)
    banner = ""
    if failed:
        banner = ('<p class="err">Couldn\'t connect to %s. Check the password '
                  "or pick another network.</p>" % html.escape(failed))
    return f"""<!doctype html><html><head><meta charset="utf-8"><title>Wi-Fi setup</title>
<style>{CSS}</style></head><body><div class="wrap">
 <div>
  <h1>Wi-Fi setup needed</h1>
  {banner}
  <ol>
   <li>On your phone, scan the QR code<br>or join Wi-Fi <code>{s}</code><br>password <code>{p}</code></li>
   <li>A setup page opens (if not, go to <code>http://10.41.0.1</code>)</li>
   <li>Pick your home Wi-Fi and enter its password</li>
  </ol>
  <p class="hint">Jellyfin comes back on its own once connected.</p>
 </div>
 <div class="qr">{wifi_qr_svg(ssid, password)}</div>
</div></body></html>"""


def page_connecting(ssid):
    """No QR and no password: this page only ever carries the network name."""
    return f"""<!doctype html><html><head><meta charset="utf-8"><title>Connecting</title>
<style>{CSS}</style></head><body><div class="wrap">
 <div style="text-align:center">
  <div class="spin"></div>
  <div class="big">Connecting to {html.escape(ssid)}&hellip;</div>
  <p class="hint">This takes a few seconds. If it comes back, the password was probably wrong.</p>
 </div>
</div></body></html>"""


def get_target():
    """The first DevTools 'page' target, or None when DevTools is unreachable."""
    try:
        targets = json.load(urllib.request.urlopen(DEVTOOLS, timeout=5))
    except (urllib.error.URLError, OSError, ValueError):
        return None
    for t in targets:
        if t.get("type") == "page":
            return t
    return None


def navigate(url):
    """Point the TV at url. 0 = shown (or already shown), 1 = something is wrong."""
    target = get_target()
    if target is None or "webSocketDebuggerUrl" not in target:
        return 1
    if target.get("url") == url:
        return 0  # the pages are built from the same inputs, so re-running with
                  # the same state is a no-op and a state change does navigate
    try:
        import websocket

        ws = websocket.create_connection(target["webSocketDebuggerUrl"], timeout=10, suppress_origin=True)
        ws.send(json.dumps({"id": 1, "method": "Page.navigate", "params": {"url": url}}))
        ws.recv()
        ws.close()
    except Exception:
        return 1
    return 0


def data_url(html_text):
    return "data:text/html;charset=utf-8," + urllib.parse.quote(html_text)


def main(argv):
    if len(argv) < 2:
        return 2
    mode = argv[1]
    if mode == "shown":
        # Is the TV showing one of our pages? Our pages are the only data: URLs
        # this window can be on, so Jellyfin itself answers 1 and the callback
        # knows it is safe (and needed) to restart the kiosk.
        target = get_target()
        return 0 if target and target.get("url", "").startswith("data:") else 1
    if mode == "connecting":
        if len(argv) < 3:
            return 2
        return navigate(data_url(page_connecting(argv[2])))
    if mode == "hotspot":
        if len(argv) < 4:
            return 2
        failed = argv[4] if len(argv) > 4 else None
        return navigate(data_url(page(argv[2], argv[3], failed)))
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv))
    except Exception:
        sys.exit(1)   # the caller logs nothing: a clean exit code is the signal
