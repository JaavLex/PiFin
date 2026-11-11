#!/usr/bin/env python3
"""PiFin phone remote.

Serves a remote-control web app on the local network (port 8080) and drives
Jellyfin Desktop:
  * navigation keys  -> virtual keyboard (uinput), works even before login
  * playback/volume  -> Jellyfin remote commands sent to our own session
  * text / search    -> typed into the focused field through DevTools
  * now playing      -> read from the Jellyfin web client, artwork proxied

At boot it also overlays a hint on the TV (QR code + address + Dismiss button)
until someone opens the remote or dismisses it with the TV remote.

DevTools: Jellyfin Desktop listens on 127.0.0.1:9222 (local only).
"""
import io
import json
import pathlib
import subprocess
import threading
import time
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import qrcode
import qrcode.image.svg
import websocket
from evdev import UInput, ecodes as e

from mpvplayer import JS_PLAYBACK_INFO, Player, js_report, js_save_position

PORT = 8080
DEVTOOLS = "http://127.0.0.1:9222/json/list"
HERE = pathlib.Path(__file__).parent
INDEX = (HERE / "index.html").read_bytes()

KEYS = {
    "up": e.KEY_UP, "down": e.KEY_DOWN, "left": e.KEY_LEFT, "right": e.KEY_RIGHT,
    "ok": e.KEY_ENTER, "back": e.KEY_ESC, "backspace": e.KEY_BACKSPACE,
}
ui = UInput({e.EV_KEY: sorted(set(KEYS.values()))}, name="pifin-phone-remote")
ui_lock = threading.Lock()

# Jellyfin remote commands we expose (sent to our own session)
PLAYSTATE = {"playpause": "PlayPause", "stop": "Stop", "next": "NextTrack", "prev": "PreviousTrack"}
GENERAL = {"home": "GoHome", "menu": "ToggleContextMenu", "osd": "ToggleOsd",
           "settings": "GoToSettings", "volup": "VolumeUp", "voldown": "VolumeDown", "mute": "ToggleMute"}

state = {"overlay": True, "client_seen": False}
player = Player()


# ---------------------------------------------------------------- DevTools --
def page_target():
    targets = json.load(urllib.request.urlopen(DEVTOOLS, timeout=3))
    return next(t for t in targets if t.get("type") == "page")


def cdp(method, params=None, target=None):
    target = target or page_target()
    ws = websocket.create_connection(target["webSocketDebuggerUrl"], timeout=8, suppress_origin=True)
    try:
        ws.send(json.dumps({"id": 1, "method": method, "params": params or {}}))
        while True:
            msg = json.loads(ws.recv())
            if msg.get("id") == 1:
                return msg
    finally:
        ws.close()


def evaluate(expr, target=None):
    msg = cdp("Runtime.evaluate", {"expression": expr, "awaitPromise": True, "returnByValue": True}, target)
    res = msg.get("result", {})
    if "exceptionDetails" in res:
        raise RuntimeError(res["exceptionDetails"].get("text", "js error"))
    return res.get("result", {}).get("value")


# Helper available in the page: own Jellyfin session
JS_SESSION = """
async function __pifinSession() {
  const a = window.ApiClient;
  if (!a || !a.accessToken()) return null;
  const s = await a.getSessions({});
  return s.find(x => x.DeviceId === a.deviceId()) || null;
}
"""


def jellyfin_command(kind, name, extra=None):
    extra_js = json.dumps(extra or {})
    if kind == "play":
        call = f"a.sendPlayStateCommand(s.Id, {json.dumps(name)}, {extra_js})"
    else:
        call = f"a.sendCommand(s.Id, {{Name: {json.dumps(name)}, Arguments: {extra_js}}})"
    return evaluate(f"""(async () => {{ {JS_SESSION}
      const a = window.ApiClient; const s = await __pifinSession();
      if (!s) return "not-logged-in"; await {call}; return "ok"; }})()""")


def now_playing():
    return evaluate(f"""(async () => {{ {JS_SESSION}
      const s = await __pifinSession();
      if (!s) return {{loggedIn: !!(window.ApiClient && window.ApiClient.accessToken())}};
      const it = s.NowPlayingItem, ps = s.PlayState || {{}};
      if (!it) return {{loggedIn: true, playing: false}};
      return {{loggedIn: true, playing: true, id: it.Id, name: it.Name, series: it.SeriesName || null,
        season: it.ParentIndexNumber ?? null, episode: it.IndexNumber ?? null, year: it.ProductionYear || null,
        type: it.Type, runTicks: it.RunTimeTicks || 0, posTicks: ps.PositionTicks || 0,
        paused: !!ps.IsPaused, muted: !!ps.IsMuted, volume: ps.VolumeLevel ?? null,
        hasImage: !!(it.ImageTags && it.ImageTags.Primary) || !!it.SeriesId,
        imageId: (it.ImageTags && it.ImageTags.Primary) ? it.Id : (it.SeriesId || it.Id)}};
    }})()""")


def send_text(text, submit=False):
    """Type text into the focused input (or the first visible text field).

    Uses real input events (Input.insertText): the Jellyfin search page is a
    React component and ignores values set from script.
    """
    target = page_target()
    found = evaluate("""(() => {
      const visible = x => x && x.offsetParent !== null;
      let el = document.activeElement;
      if (!visible(el) || !/INPUT|TEXTAREA/.test(el.tagName))
        el = [...document.querySelectorAll('#searchTextInput, input[type=search], input[type=text], input[type=password], textarea')].find(visible);
      if (!el) return false;
      el.focus(); if (el.select) el.select();   // replace what's there
      return true;
    })()""", target)
    if not found:
        return "no-input"
    cdp("Input.insertText", {"text": text}, target)
    if submit:
        for kind in ("keyDown", "keyUp"):
            cdp("Input.dispatchKeyEvent", {"type": kind, "key": "Enter", "code": "Enter",
                                           "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13}, target)
    return "ok"


def search(text):
    """Search the library; results are shown on the phone (with posters).

    Typing into the TV's own search page doesn't work reliably: it resets its
    box after loading. Searching through the API and letting the phone pick a
    result to play or open on the TV is also nicer to use.
    """
    return evaluate(f"""(async () => {{
      const a = window.ApiClient;
      if (!a || !a.accessToken()) return {{error: "not-logged-in"}};
      const r = await a.getItems(a.getCurrentUserId(), {{
        searchTerm: {json.dumps(text)}, Recursive: true, Limit: 30,
        IncludeItemTypes: "Movie,Series,Episode,BoxSet",
        Fields: "ProductionYear", EnableTotalRecordCount: false }});
      return {{items: r.Items.map(i => ({{
        id: i.Id, name: i.Name, type: i.Type, year: i.ProductionYear || null,
        series: i.SeriesName || null, season: i.ParentIndexNumber ?? null, episode: i.IndexNumber ?? null,
        imageId: (i.ImageTags && i.ImageTags.Primary) ? i.Id : (i.SeriesId || null) }}))}};
    }})()""")


def open_item(item_id):
    """Show the item's page on the TV."""
    return evaluate(f"""(() => {{
      const a = window.ApiClient;
      if (!a || !a.accessToken()) return "not-logged-in";
      location.hash = '#/details?id=' + encodeURIComponent({json.dumps(item_id)}) + '&serverId=' + a.serverId();
      return "ok"; }})()""")


def play_item(item_id):
    return evaluate(f"""(async () => {{ {JS_SESSION}
      const a = window.ApiClient; const s = await __pifinSession();
      if (!s) return "not-logged-in";
      await a.sendPlayCommand(s.Id, {{ItemIds: {json.dumps(item_id)}, PlayCommand: "PlayNow"}});
      return "ok"; }})()""")


def artwork(item_id):
    info = evaluate("window.ApiClient ? [window.ApiClient.serverAddress(), window.ApiClient.accessToken()] : null")
    if not info:
        return None
    server, token = info
    url = f"{server}/Items/{urllib.parse.quote(item_id)}/Images/Primary?maxHeight=480&quality=85"
    req = urllib.request.Request(url, headers={"Authorization": f'MediaBrowser Token="{token}"'})
    with urllib.request.urlopen(req, timeout=8) as r:
        return r.read(), r.headers.get("Content-Type", "image/jpeg")


# ----------------------------------------------------------- TV overlay ----
def local_ip():
    try:
        out = subprocess.run(["ip", "-4", "route", "get", "1.1.1.1"], capture_output=True, text=True).stdout
        return out.split(" src ")[1].split()[0]
    except Exception:
        return None


def qr_svg(text):
    img = qrcode.make(text, image_factory=qrcode.image.svg.SvgPathImage, box_size=10, border=1)
    buf = io.BytesIO()
    img.save(buf)
    return buf.getvalue().decode()


OVERLAY_JS = r"""
(() => {
  if (window.__pifinDismissed || document.getElementById('pifin-hint')) return 'present';
  const d = document.createElement('div');
  d.id = 'pifin-hint';
  d.innerHTML = `
  <style>
    #pifin-hint{position:fixed;inset:0;z-index:2147483647;display:flex;align-items:center;justify-content:center;
      background:rgba(6,6,12,.82);backdrop-filter:blur(6px);font-family:'Noto Sans',sans-serif;color:#f2f2f7}
    #pifin-hint .card{display:flex;gap:5vh;align-items:center;background:#16161f;border:1px solid #2c2c3d;
      border-radius:3vh;padding:5vh 6vh;box-shadow:0 2vh 8vh rgba(0,0,0,.5)}
    #pifin-hint .qr{background:#fff;border-radius:2vh;padding:1.6vh;width:30vh;height:30vh}
    #pifin-hint .qr svg{width:100%;height:100%}
    #pifin-hint h2{margin:0 0 2vh;font-size:4.6vh;background:linear-gradient(90deg,#aa5cc3,#00a4dc);
      -webkit-background-clip:text;color:transparent}
    #pifin-hint p{margin:0 0 1.4vh;font-size:2.8vh;color:#c9c9d6;max-width:60vh}
    #pifin-hint code{font-size:3.4vh;color:#fff;background:#24243a;padding:.3vh 1.2vh;border-radius:1vh}
    #pifin-hint button{margin-top:3vh;font-size:2.8vh;padding:1.4vh 4vh;border-radius:5vh;border:0;color:#fff;
      background:linear-gradient(90deg,#aa5cc3,#00a4dc);cursor:pointer;outline:none}
    #pifin-hint button:focus{box-shadow:0 0 0 .6vh rgba(255,255,255,.85)}
  </style>
  <div class="card">
    <div class="qr">__QR__</div>
    <div>
      <h2>Use your phone as a remote</h2>
      <p>Connect your phone to this Wi-Fi, then scan the code or open</p>
      <p><code>__URL__</code></p>
      <p>Or just use your TV remote or air-mouse remote.</p>
      <button id="pifin-dismiss">Dismiss</button>
    </div>
  </div>`;
  document.body.appendChild(d);
  const close = () => { window.__pifinDismissed = true; d.remove(); window.removeEventListener('keydown', trap, true); };
  const trap = ev => {
    if (!document.getElementById('pifin-hint')) return;
    if (['Enter', 'NumpadEnter', 'BrowserBack', 'Escape', 'Backspace', 'GoBack'].includes(ev.key)) close();
    ev.stopImmediatePropagation(); ev.preventDefault();
  };
  window.addEventListener('keydown', trap, true);
  d.querySelector('#pifin-dismiss').addEventListener('click', close);
  setTimeout(() => d.querySelector('#pifin-dismiss').focus(), 50);
  return 'shown';
})()
"""


def overlay_loop():
    while True:
        time.sleep(3)
        try:
            target = page_target()
            url = target.get("url", "")
            if not url.startswith("http"):
                continue  # Wi-Fi setup screen or Jellyfin not loaded yet
            if not state["overlay"]:
                evaluate("(() => { const d = document.getElementById('pifin-hint'); if (d) { window.__pifinDismissed = true; d.remove(); } })()", target)
                continue
            if evaluate("!!window.__pifinDismissed", target):
                state["overlay"] = False
                continue
            ip = local_ip()
            if not ip:
                continue
            link = f"http://{ip}:{PORT}"
            js = OVERLAY_JS.replace("__QR__", qr_svg(link).replace("`", "")).replace("__URL__", link)
            evaluate(js, target)
        except Exception:
            pass


# ------------------------------------------------------ mpv playback handoff --
def handoff_loop():
    """When Jellyfin starts playing, stop it and play in native mpv instead."""
    last_report = 0
    while True:
        time.sleep(0.5)
        try:
            if player.running():
                if time.time() - last_report > 10 and player.info:
                    pos = player.get("time-pos")
                    if pos is not None:
                        evaluate(js_report("progress", player.info, pos * 1e7, player.get("pause")))
                        evaluate(js_save_position(player.info, pos * 1e7))
                    last_report = time.time()
                continue
            if player.info:  # mpv just exited: final report, back to the UI
                info, player.info = player.info, None
                last = info.get("_last_pos", 0)
                evaluate(js_report("stop", info, last))
                time.sleep(1)  # let the stop report land, then set the real position
                evaluate(js_save_position(info, last))
                continue
            if not str(evaluate("location.hash") or "").startswith("#/video"):
                continue
            info = evaluate(JS_PLAYBACK_INFO)
            if not info:
                continue
            # stop the built-in (software) player, keep the UI on the item page
            evaluate(f"(async () => {{ await window.ApiClient.sendPlayStateCommand({json.dumps(info['sessionId'])}, 'Stop'); return 1; }})()")
            time.sleep(0.8)
            player.start(info)
            time.sleep(2)
            evaluate(js_report("start", info, info["pos"]))
            last_report = time.time()
        except Exception:
            pass


def position_tracker():
    """Remember the last mpv position so the stop report is accurate."""
    while True:
        time.sleep(1)
        if player.running() and player.info:
            pos = player.get("time-pos")
            if pos is not None:
                player.info["_last_pos"] = pos * 1e7


# ------------------------------------------------------------- HTTP API ----
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code=200, body=b"", ctype="application/json", cache=False):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "max-age=3600" if cache else "no-store")
        self.end_headers()
        self.wfile.write(body)

    def json(self, obj, code=200):
        self.reply(code, json.dumps(obj).encode())

    def seen(self):
        if not state["client_seen"]:
            state["client_seen"] = True
        state["overlay"] = False

    def do_GET(self):
        path = urllib.parse.urlparse(self.path)
        if path.path in ("/", "/index.html"):
            self.seen()
            return self.reply(200, INDEX, "text/html; charset=utf-8")
        if path.path == "/manifest.webmanifest":
            m = {"name": "PiFin Remote", "short_name": "Remote", "start_url": "/", "display": "standalone",
                 "background_color": "#0b0b12", "theme_color": "#0b0b12",
                 "icons": [{"src": "/icon.svg", "sizes": "any", "type": "image/svg+xml"}]}
            return self.reply(200, json.dumps(m).encode(), "application/manifest+json", cache=True)
        if path.path == "/icon.svg":
            svg = (b'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><defs><linearGradient id="g" x1="0" x2="1">'
                   b'<stop offset="0" stop-color="#aa5cc3"/><stop offset="1" stop-color="#00a4dc"/></linearGradient></defs>'
                   b'<rect width="64" height="64" rx="14" fill="#0b0b12"/><circle cx="32" cy="32" r="20" fill="none" stroke="url(#g)" stroke-width="6"/>'
                   b'<circle cx="32" cy="32" r="7" fill="url(#g)"/></svg>')
            return self.reply(200, svg, "image/svg+xml", cache=True)
        if path.path == "/api/status":
            self.seen()
            if player.running():
                try:
                    return self.json({"ok": True, **player.status()})
                except Exception as ex:
                    return self.json({"ok": False, "error": str(ex)})
            try:
                return self.json({"ok": True, **(now_playing() or {})})
            except Exception as ex:
                return self.json({"ok": False, "error": str(ex)})
        if path.path.startswith("/api/art/"):
            try:
                art = artwork(path.path.rsplit("/", 1)[1])
                if art:
                    return self.reply(200, art[0], art[1], cache=True)
            except Exception:
                pass
            return self.reply(404, b"", "text/plain")
        self.reply(404, b"not found", "text/plain")

    def do_POST(self):
        self.seen()
        try:
            n = int(self.headers.get("Content-Length") or 0)
            data = json.loads(self.rfile.read(n) or b"{}")
            action = data.get("action", "")
            if player.running():
                result = player.action(action, data)
                if result != "ignored":
                    return self.json({"ok": True, "result": result})
            if action in KEYS:
                with ui_lock:
                    ui.write(e.EV_KEY, KEYS[action], 1); ui.syn()
                    ui.write(e.EV_KEY, KEYS[action], 0); ui.syn()
                return self.json({"ok": True})
            if action in PLAYSTATE:
                return self.json({"ok": True, "result": jellyfin_command("play", PLAYSTATE[action])})
            if action in GENERAL:
                return self.json({"ok": True, "result": jellyfin_command("general", GENERAL[action])})
            if action == "seek":
                ticks = int(data["ticks"])
                return self.json({"ok": True, "result": jellyfin_command("play", "Seek", {"SeekPositionTicks": max(0, ticks)})})
            if action == "text":
                return self.json({"ok": True, "result": send_text(str(data.get("text", "")), bool(data.get("submit")))})
            if action == "search":
                res = search(str(data.get("text", ""))) or {}
                return self.json({"ok": "error" not in res, "result": res.get("error", "ok"), "items": res.get("items", [])})
            if action == "open":
                return self.json({"ok": True, "result": open_item(str(data["id"]))})
            if action == "playitem":
                return self.json({"ok": True, "result": play_item(str(data["id"]))})
            return self.json({"ok": False, "error": "unknown action"}, 400)
        except Exception as ex:
            return self.json({"ok": False, "error": str(ex)}, 500)


if __name__ == "__main__":
    threading.Thread(target=overlay_loop, daemon=True).start()
    threading.Thread(target=handoff_loop, daemon=True).start()
    threading.Thread(target=position_tracker, daemon=True).start()
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
