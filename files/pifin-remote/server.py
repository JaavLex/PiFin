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
  * settings        -> the root settings daemon's socket (/run/pifin/settings.sock)
Only paired phones get in: the TV's QR code opens /pair?t=<token>, which sets a
cookie; every other page and /api/* call needs that cookie (D8).
"""
import hmac
import http.cookies
import json
import os
import pathlib
import socket
import threading
import time
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from evdev import UInput, ecodes as e

from mpvplayer import Player

PORT = 8080
HERE = pathlib.Path(__file__).parent
INDEX = (HERE / "index.html").read_bytes()
SETTINGS_PAGE = (HERE / "settings.html").read_bytes()
ICON = (HERE / "icon.svg").read_bytes()
PAIR_TOKEN = os.environ.get("PIFIN_PAIR_TOKEN", "/var/lib/pifin/pair-token")
SETTINGS_SOCK = os.environ.get("PIFIN_SETTINGS_SOCK", "/run/pifin/settings.sock")
# what the phone may ask the daemon; pair_reset stays TV-only (it needs the TV in hand)
SETTINGS_OPS = frozenset("get set network_set wifi_list wifi_scan wifi_add wifi_forget pin_set "
                         "pin_clear pin_check tailscale_login reboot factory_reset update_check "
                         "update_install".split())
STORE = os.environ.get("PIFIN_SETTINGS", "/var/lib/pifin/settings.json")
SUB_MODES = frozenset(("Default", "Always", "OnlyForced", "None", "Smart"))
FORBIDDEN = (b'<!doctype html><html lang="en"><head><meta charset="utf-8">'
             b'<meta name="viewport" content="width=device-width, initial-scale=1"><title>PiFin</title></head>'
             b'<body style="margin:0;min-height:100vh;display:grid;place-items:center;background:#0b0b12;'
             b'color:#f2f2f7;font:17px system-ui,sans-serif;padding:24px;box-sizing:border-box;text-align:center">'
             b'<p>Scan the QR code on the TV (Settings > System > Phone remote).</p></body></html>')
PAIRED = (b'<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="refresh" content="0;url=/">'
          b'<title>PiFin</title></head><body style="background:#0b0b12"></body></html>')
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
state = {"seen": False, "phones": {}, "bye_at": None}
state_lock = threading.Lock()
# The TV shows its QR code again only once every phone has left the remote page (the page
# says "bye" when it is closed or navigated away; a phone that sleeps or switches apps
# says nothing and stays connected). A short grace lets a reload or a move to /settings
# come back first. A phone silent for STALE_AFTER (browser killed, phone gone) is dropped.
BYE_GRACE = 5
STALE_AFTER = 6 * 3600


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


def seen_now(now, phone):
    """A paired phone made a request: on first contact the TV hides its QR hint."""
    with state_lock:
        state["phones"][phone] = now
        if not state["seen"]:
            state["seen"] = app_cmd("seen") == "ok"


def bye(now, phone):
    """The phone left the remote page (closed, reloaded, or moved to another page)."""
    with state_lock:
        state["phones"].pop(phone, None)
        state["bye_at"] = now


def check_gone(now):
    """Every phone left (or went stale): the TV shows its QR hint again."""
    with state_lock:
        phones = state["phones"]
        for phone, last in list(phones.items()):
            if now - last > STALE_AFTER:
                del phones[phone]
        if not state["seen"] or phones:
            return
        if state["bye_at"] is not None and now - state["bye_at"] < BYE_GRACE:
            return
        state["seen"] = app_cmd("unseen") != "ok"


def watch_phones():
    while True:
        time.sleep(2)
        check_gone(time.monotonic())


def mpv_alive():
    return player.get("pid") is not None


def now_playing():
    try:
        info = json.loads(pathlib.Path(NOW).read_text())
    except (OSError, ValueError):
        info = {}
    player.info = info
    st = player.status()
    # Jellyfin's own track names ("English - DTS-HD MA - 5.1"), listed by the app
    # in mpv's order; mpv's guesses are kept if the counts don't match
    names = info.get("names") or {}
    for kind in ("audio", "sub"):
        tracks, labels = st.get("tracks", {}).get(kind, []), names.get(kind) or []
        if tracks and len(tracks) == len(labels):
            for t, label in zip(tracks, labels):
                t["label"] = label
    return st


# ------------------------------------------------------------- pairing -----
_token = {"key": None, "value": None}


def pair_token():
    """The current pairing token, re-read when the file changes (Re-pair on the TV)."""
    try:
        st = os.stat(PAIR_TOKEN)
        key = (st.st_ino, st.st_mtime_ns, st.st_size)
        if key != _token["key"]:
            _token["value"] = pathlib.Path(PAIR_TOKEN).read_text().strip() or None
            _token["key"] = key
    except OSError:
        return None
    return _token["value"]


def token_ok(given):
    """Constant-time check of a token from the QR link or the cookie."""
    token = pair_token()
    return bool(token and given) and hmac.compare_digest(given.encode(), token.encode())


# ------------------------------------------------------------ settings ------
def daemon(req):
    """One request to the settings daemon; its JSON reply line comes back as is."""
    try:
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(70)   # tailscale_login, wifi_add ~30 s; update_check up to 60 s
            s.connect(SETTINGS_SOCK)
            s.sendall((json.dumps(req) + "\n").encode())
            buf = b""
            while not buf.endswith(b"\n") and len(buf) < 4 * 1024 * 1024:
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
    except socket.timeout:
        buf = b""
    except OSError:
        return json.dumps({"ok": False, "error": "failed", "message": "Settings service not running."}).encode()
    return buf.strip() or json.dumps({"ok": False, "error": "failed",
                                      "message": "Settings service did not answer."}).encode()


def setup_needed():
    """A fresh image: no Jellyfin server address yet, so the phone opens the setup wizard."""
    try:
        doc = json.loads(pathlib.Path(STORE).read_text())
        return not doc["system"]["jellyfin_url"]
    except (OSError, ValueError, KeyError, TypeError):
        return False  # no store (yet) or unreadable: the remote, as before


def server_test(url):
    """Can PiFin reach this Jellyfin server? Asked from the Pi, not the phone: only the Pi's
    network matters. Answers the server's name and version, nothing else."""
    if not isinstance(url, str) or not url.startswith(("http://", "https://")) or len(url) > 300:
        return {"ok": False, "error": "invalid_value", "message": "The address must look like http://host:8096."}
    try:
        req = urllib.request.Request(url.rstrip("/") + "/System/Info/Public", headers={"User-Agent": "PiFin"})
        with urllib.request.urlopen(req, timeout=6) as r:
            info = json.loads(r.read(64 * 1024))
        name, version = info.get("ServerName"), info.get("Version")
        if not isinstance(version, str):
            raise ValueError("not Jellyfin")
        return {"ok": True, "result": {"name": str(name or "")[:80], "version": version[:20]}}
    except Exception:
        return {"ok": False, "error": "failed",
                "message": "No Jellyfin server answered at that address. Check it, or sign in to Tailscale first if the server is on your tailnet."}


# ------------------------------------------------------------ Jellyfin ------
def auth():
    return json.loads(pathlib.Path(AUTH).read_text())


def _request(path, query=None, data=None):
    a = auth()
    url = a["server"].rstrip("/") + path + ("?" + urllib.parse.urlencode(query) if query else "")
    headers = {"Authorization": f'MediaBrowser Client="PiFin Remote", Device="Raspberry Pi", '
                                f'DeviceId="{a["device_id"]}", Version="1", Token="{a["token"]}"'}
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data).encode()
    return urllib.request.Request(url, data=data, headers=headers)


def api(path, query=None, raw=False):
    with urllib.request.urlopen(_request(path, query), timeout=10) as r:
        body = r.read(8 * 1024 * 1024)
        return (body, r.headers.get("Content-Type", "image/jpeg")) if raw else json.loads(body)


def post_api(path, obj, query=None):
    with urllib.request.urlopen(_request(path, query, obj), timeout=10) as r:
        r.read(1024 * 1024)


_cultures = []


def cultures():
    """The server's languages as [code, name], by name, no duplicates (cached)."""
    if not _cultures:
        seen = {}
        for c in api("/Localization/Cultures"):
            code, name = c.get("ThreeLetterISOLanguageName"), c.get("DisplayName")
            if code and name and code not in seen:
                seen[code] = name
        _cultures.extend(sorted(([k, v] for k, v in seen.items()), key=lambda kv: kv[1].lower()))
    return _cultures


def langs():
    """The active user's language keys (D7), read from their Jellyfin Configuration."""
    me = api("/Users/Me")
    cfg = me.get("Configuration") or {}
    return {"ok": True, "user": me.get("Name", ""), "audio": cfg.get("AudioLanguagePreference") or "",
            "subtitle": cfg.get("SubtitleLanguagePreference") or "", "mode": cfg.get("SubtitleMode") or "Default",
            "cultures": cultures()}


def set_langs(data):
    """Write the three keys into the full Configuration (the server replaces it whole)."""
    audio, sub, mode = data.get("audio"), data.get("subtitle"), data.get("mode")
    for code in (audio, sub):
        if not isinstance(code, str) or len(code) > 8 or not all(ch.isalnum() or ch == "-" for ch in code):
            return {"ok": False, "error": "invalid_value", "message": "Unknown language."}
    if mode not in SUB_MODES:
        return {"ok": False, "error": "invalid_value", "message": "Unknown subtitle mode."}
    me = api("/Users/Me")
    cfg = me.get("Configuration") or {}
    cfg.update(AudioLanguagePreference=audio, SubtitleLanguagePreference=sub, SubtitleMode=mode)
    post_api("/Users/Configuration", cfg, {"userId": me["Id"]})
    return {"ok": True, "user": me.get("Name", "")}


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

    def reply(self, code=200, body=b"", ctype="application/json", cache=False, headers=()):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "max-age=3600" if cache else "no-store")
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def json(self, obj, code=200):
        self.reply(code, json.dumps(obj).encode())

    def paired(self):
        """The request carries the pifin cookie and it equals the current token."""
        jar = http.cookies.SimpleCookie()
        try:
            jar.load(self.headers.get("Cookie") or "")
        except http.cookies.CookieError:
            return False
        morsel = jar.get("pifin")
        return morsel is not None and token_ok(morsel.value)

    def forbidden(self):
        # the request body (if any) is not read: close so it can't be taken for the next request
        self.close_connection = True
        self.reply(403, FORBIDDEN, "text/html; charset=utf-8", headers=[("Connection", "close")])

    def body(self, limit=64 * 1024):
        try:
            n = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            n = -1
        if not 0 <= n <= limit:
            # rfile.read(-1) would block until the client hangs up; the body stays unread,
            # so the connection must not be reused
            self.close_connection = True
            raise ValueError("bad Content-Length")
        return json.loads(self.rfile.read(n) or b"{}")

    def seen(self):
        seen_now(time.monotonic(), self.client_address[0])

    def do_GET(self):
        path = urllib.parse.urlparse(self.path)
        if path.path == "/pair":
            t = urllib.parse.parse_qs(path.query).get("t", [""])[0]
            if not token_ok(t):
                return self.forbidden()
            # 200 + meta refresh rather than a redirect: some phone browsers drop a
            # cookie set on a 302 when the QR scanner hands the link over
            # from the token just validated: a second read could see a newer or missing file
            cookie = f"pifin={t}; HttpOnly; SameSite=Strict; Max-Age=31536000; Path=/"
            return self.reply(200, PAIRED, "text/html; charset=utf-8", headers=[("Set-Cookie", cookie)])
        if path.path not in ("/manifest.webmanifest", "/icon.svg") and not self.paired():
            return self.forbidden()
        if path.path in ("/", "/index.html"):
            self.seen()
            if setup_needed():  # first setup: the wizard is on the settings page
                return self.reply(200, SETTINGS_PAGE, "text/html; charset=utf-8")
            return self.reply(200, INDEX, "text/html; charset=utf-8")
        if path.path == "/settings":
            self.seen()
            return self.reply(200, SETTINGS_PAGE, "text/html; charset=utf-8")
        if path.path == "/api/langs":
            try:
                return self.json(langs())
            except Exception:
                return self.json({"ok": False, "error": "failed", "message": "Jellyfin server not reachable."})
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
        if not self.paired():
            return self.forbidden()
        path = urllib.parse.urlparse(self.path).path
        if path == "/api/bye":  # sendBeacon from the page's pagehide
            bye(time.monotonic(), self.client_address[0])
            return self.json({"ok": True})
        self.seen()
        if path == "/api/settings":
            try:
                data = self.body()
            except ValueError:
                data = None
            if not isinstance(data, dict) or data.get("op") not in SETTINGS_OPS:
                return self.json({"ok": False, "error": "bad_request", "message": "Unknown op."}, 400)
            data["via"] = "phone"  # set here, never by the client: the daemon is stricter with phones
            return self.reply(200, daemon(data))
        if path == "/api/server_test":
            try:
                data = self.body()
            except ValueError:
                data = None
            return self.json(server_test(data.get("url") if isinstance(data, dict) else None))
        if path == "/api/langs":
            try:
                data = self.body()
                if not isinstance(data, dict):
                    raise ValueError("not an object")
            except ValueError:
                return self.json({"ok": False, "error": "bad_request", "message": "Expected a JSON object."}, 400)
            try:
                return self.json(set_langs(data))
            except Exception:
                return self.json({"ok": False, "error": "failed", "message": "Jellyfin server not reachable."})
        try:
            data = self.body(1024 * 1024)
        except ValueError:
            return self.json({"ok": False, "error": "bad_request"}, 400)
        try:
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
    threading.Thread(target=watch_phones, daemon=True).start()
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
