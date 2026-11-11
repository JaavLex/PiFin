#!/usr/bin/env python3
"""Show the Wi-Fi setup instructions full screen on the TV.

Drives the Jellyfin Desktop window through its local DevTools port
(127.0.0.1:9222) and navigates it to a self-contained instructions page with a
QR code to join the setup hotspot. Getting back to Jellyfin is done by
restarting jellyfin-kiosk (see pifin-comitup-callback).

Usage: pifin-setup-screen.py <hotspot-ssid> <hotspot-password>
"""
import html
import io
import json
import sys
import urllib.parse
import urllib.request

import qrcode
import qrcode.image.svg
import websocket

DEVTOOLS = "http://127.0.0.1:9222/json/list"


def wifi_qr_svg(ssid, password):
    esc = lambda s: "".join("\\" + c if c in '\\;,:"' else c for c in s)
    payload = f"WIFI:T:WPA;S:{esc(ssid)};P:{esc(password)};;"
    img = qrcode.make(payload, image_factory=qrcode.image.svg.SvgPathImage, box_size=12, border=2)
    buf = io.BytesIO()
    img.save(buf)
    return buf.getvalue().decode()


def page(ssid, password):
    s, p = html.escape(ssid), html.escape(password)
    return f"""<!doctype html><html><head><meta charset="utf-8"><title>Wi-Fi setup</title>
<style>
 html,body{{margin:0;height:100%;background:#101018;color:#f2f2f7;font-family:sans-serif}}
 .wrap{{display:flex;height:100%;align-items:center;justify-content:center;gap:6vw;padding:0 6vw;box-sizing:border-box}}
 h1{{font-size:5vh;margin:0 0 4vh;color:#aa5cc3}}
 ol{{font-size:3.4vh;line-height:1.7;margin:0;padding-left:1.2em}}
 code{{background:#2a2a3a;padding:.1em .4em;border-radius:.3em;color:#fff}}
 .qr{{background:#fff;padding:2vh;border-radius:2vh;width:38vh;height:38vh}}
 .qr svg{{width:100%;height:100%}}
 .hint{{font-size:2.4vh;color:#9a9ab0;margin-top:4vh}}
</style></head><body><div class="wrap">
 <div>
  <h1>Wi-Fi setup needed</h1>
  <ol>
   <li>On your phone, scan the QR code<br>or join Wi-Fi <code>{s}</code><br>password <code>{p}</code></li>
   <li>A setup page opens (if not, go to <code>http://10.41.0.1</code>)</li>
   <li>Pick your home Wi-Fi and enter its password</li>
  </ol>
  <p class="hint">Jellyfin comes back on its own once connected.</p>
 </div>
 <div class="qr">{wifi_qr_svg(ssid, password)}</div>
</div></body></html>"""


def main():
    ssid, password = sys.argv[1], sys.argv[2]
    targets = json.load(urllib.request.urlopen(DEVTOOLS, timeout=5))
    target = next(t for t in targets if t.get("type") == "page")
    if target.get("url", "").startswith("data:"):
        return  # already showing
    url = "data:text/html;charset=utf-8," + urllib.parse.quote(page(ssid, password))
    ws = websocket.create_connection(target["webSocketDebuggerUrl"], timeout=10, suppress_origin=True)
    ws.send(json.dumps({"id": 1, "method": "Page.navigate", "params": {"url": url}}))
    ws.recv()
    ws.close()


if __name__ == "__main__":
    main()
