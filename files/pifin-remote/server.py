#!/usr/bin/env python3
"""PiFin phone remote.

Serves a remote-control web app on the local network (port 8080) and drives
the PiFin app and the mpv it starts:
  * navigation keys -> virtual keyboard (uinput); the app and mpv both read it
  * while mpv plays -> playback, seek, volume, audio/subtitle tracks over mpv's
                       IPC socket; now-playing comes from mpv + the app's
                       /tmp/pifin-now.json
  * search, posters -> the Jellyfin API with the app's login
  * open / play     -> commands to the app's socket (/tmp/pifin-app.sock)
"""
import json
import os
import pathlib
import socket
import threading
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from evdev import UInput, ecodes as e

from mpvplayer import Player

PORT = 8080
HERE = pathlib.Path(__file__).parent
INDEX = (HERE / "index.html").read_bytes()
ICON = (HERE / "icon.svg").read_bytes()
AUTH = os.environ.get("PIFIN_AUTH", "/home/pifin/.config/pifin/auth.json")
NOW = "/tmp/pifin-now.json"
APP_SOCK = "/tmp/pifin-app.sock"

KEYS = {
    "up": e.KEY_UP, "down": e.KEY_DOWN, "left": e.KEY_LEFT, "right": e.KEY_RIGHT,
    "ok": e.KEY_ENTER, "back": e.KEY_ESC, "backspace": e.KEY_BACKSPACE,
}
ui = UInput({e.EV_KEY: sorted(set(KEYS.values()))}, name="pifin-phone-remote")
ui_lock = threading.Lock()
player = Player()   # used only for its mpv IPC helpers; the app starts mpv
state = {"seen": False}


def press(key):
    with ui_lock:
        ui.write(e.EV_KEY, KEYS[key], 1); ui.syn()
        ui.write(e.EV_KEY, KEYS[key], 0); ui.syn()


# ------------------------------------------------------------- the app -----
def app_cmd(line):
    """Send one command line to the PiFin app ("play <id>", "open <id>", "home", "seen")."""
    try:
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(2)
        s.connect(APP_SOCK)
        s.sendall((line + "\n").encode())
        s.close()
        return "ok"
    except OSError:
        return "app-not-running"


def mpv_alive():
    return player.get("pid") is not None


def now_playing():
    try:
        info = json.loads(pathlib.Path(NOW).read_text())
    except (OSError, ValueError):
        info = {}
    player.info = info
    return player.status()


# ------------------------------------------------------------ Jellyfin ------
def auth():
    return json.loads(pathlib.Path(AUTH).read_text())


def api(path, query=None, raw=False):
    a = auth()
    url = a["server"].rstrip("/") + path + ("?" + urllib.parse.urlencode(query) if query else "")
    req = urllib.request.Request(url, headers={
        "Authorization": f'MediaBrowser Client="PiFin Remote", Device="Raspberry Pi", '
                         f'DeviceId="{a["device_id"]}", Version="1", Token="{a["token"]}"'})
    with urllib.request.urlopen(req, timeout=10) as r:
        body = r.read(8 * 1024 * 1024)
        return (body, r.headers.get("Content-Type", "image/jpeg")) if raw else json.loads(body)


def search(text):
    """Search the library; results are shown on the phone with posters."""
    a = auth()
    r = api(f"/Users/{a['user_id']}/Items", {
        "SearchTerm": text, "Recursive": "true", "Limit": "30",
        "IncludeItemTypes": "Movie,Series,Episode", "Fields": "ProductionYear",
        "EnableTotalRecordCount": "false"})
    return [{
        "id": i["Id"], "name": i["Name"], "type": i["Type"], "year": i.get("ProductionYear"),
        "series": i.get("SeriesName"), "season": i.get("ParentIndexNumber"), "episode": i.get("IndexNumber"),
        "imageId": i["Id"] if (i.get("ImageTags") or {}).get("Primary") else i.get("SeriesId"),
    } for i in r.get("Items", [])]


def artwork(item_id):
    if not all(c.isalnum() or c == "-" for c in item_id):
        return None
    return api(f"/Items/{item_id}/Images/Primary", {"maxWidth": "300", "quality": "80"}, raw=True)


# ------------------------------------------------------------- HTTP API ----
class Handler(BaseHTTPRequestHandler):
    # keep-alive: the phone reuses one connection for every tap, so a weak Wi-Fi
    # link doesn't pay a new TCP handshake per button press
    protocol_version = "HTTP/1.1"
    timeout = 30

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
        # first contact: the TV hides its "phone remote" hint
        if not state["seen"]:
            state["seen"] = app_cmd("seen") == "ok"

    def do_GET(self):
        path = urllib.parse.urlparse(self.path)
        if path.path in ("/", "/index.html"):
            self.seen()
            return self.reply(200, INDEX, "text/html; charset=utf-8")
        if path.path == "/manifest.webmanifest":
            m = {"name": "PiFin Remote", "short_name": "PiFin", "start_url": "/", "display": "standalone",
                 "background_color": "#0b0b12", "theme_color": "#0b0b12",
                 "icons": [{"src": "/icon.svg", "sizes": "any", "type": "image/svg+xml"}]}
            return self.reply(200, json.dumps(m).encode(), "application/manifest+json", cache=True)
        if path.path == "/icon.svg":
            return self.reply(200, ICON, "image/svg+xml", cache=True)
        if path.path == "/api/status":
            self.seen()
            try:
                if mpv_alive():
                    return self.json({"ok": True, **now_playing()})
                return self.json({"ok": True, "loggedIn": os.path.exists(AUTH), "playing": False})
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
            if mpv_alive():
                result = player.action(action, data)
                if result != "ignored":
                    return self.json({"ok": True, "result": result})
            if action in KEYS:
                press(action)
                return self.json({"ok": True})
            if action == "home":
                return self.json({"ok": True, "result": app_cmd("home")})
            if action == "text":
                text = str(data.get("text", "")).replace("\n", " ")[:200]
                return self.json({"ok": True, "result": app_cmd("text " + text)})
            if action == "search":
                return self.json({"ok": True, "result": "ok", "items": search(str(data.get("text", "")))})
            if action in ("open", "playitem"):
                item = str(data.get("id", ""))
                if not all(c.isalnum() or c == "-" for c in item):
                    return self.json({"ok": False, "error": "bad id"}, 400)
                return self.json({"ok": True, "result": app_cmd(("play " if action == "playitem" else "open ") + item)})
            # nothing to control when nothing plays (pause, volume, menu, text…)
            return self.json({"ok": True, "result": "ignored"})
        except Exception as ex:
            return self.json({"ok": False, "error": str(ex)}, 500)


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
