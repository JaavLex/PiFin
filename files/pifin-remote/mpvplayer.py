"""Hand Jellyfin playback over to the Pi's native mpv.

The Flatpak Jellyfin Desktop can't use the Pi's hardware decoders, so video
stutters. When the user presses Play in the Jellyfin UI, we stop the built-in
player, read what was chosen on the episode screen (media source, audio and
subtitle track, start position) and play the original file in Raspberry Pi
OS's mpv (hardware HEVC/H.264 decoding, "fast" renderer) on top of the UI.
Progress is reported back to Jellyfin; quitting mpv returns to the UI.
"""
import json
import os
import pathlib
import socket
import subprocess
import threading
import time
import urllib.parse

SOCK = "/tmp/pifin-mpv.sock"
INPUT_CONF = str(pathlib.Path(__file__).parent / "mpv-input.conf")
SCRIPT = str(pathlib.Path(__file__).parent / "pifin-osd.lua")
SUB_FMT = {"subrip": "srt", "srt": "srt", "ass": "ass", "ssa": "ssa", "webvtt": "vtt", "vtt": "vtt"}


class Player:
    def __init__(self):
        self.proc = None
        self.info = None
        self.lock = threading.Lock()

    def running(self):
        return self.proc is not None and self.proc.poll() is None

    # ---------------------------------------------------------------- IPC --
    def _ipc(self, command):
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(2)
        try:
            s.connect(SOCK)
            s.sendall((json.dumps({"command": command}) + "\n").encode())
            buf = b""
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    return None
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    msg = json.loads(line)
                    if "event" not in msg:
                        return msg.get("data")
        finally:
            s.close()

    def cmd(self, *args):
        try:
            return self._ipc(list(args))
        except Exception:
            return None

    def get(self, prop):
        return self.cmd("get_property", prop)

    # -------------------------------------------------------------- start --
    def start(self, info):
        """info comes from the Jellyfin web client (see JS_PLAYBACK_INFO)."""
        server, token, item, msid = info["server"], info["token"], info["id"], info["msid"]
        streams = info["streams"]
        audio = [s for s in streams if s["t"] == "Audio" and not s["ext"]]
        subs_int = [s for s in streams if s["t"] == "Subtitle" and not s["ext"]]
        subs_ext = [s for s in streams if s["t"] == "Subtitle" and s["ext"]]

        args = ["--fs", "--vo=gpu", "--gpu-context=wayland", "--profile=fast",
                "--hwdec=drm,v4l2m2m-copy,no", "--cache=yes", "--demuxer-max-bytes=150MiB",
                f"--input-ipc-server={SOCK}", f"--input-conf={INPUT_CONF}",
                "--osc=no", "--osd-level=0", "--osd-scale-by-window=no",
                f"--script={SCRIPT}", "--really-quiet",
                f"--force-media-title={info.get('title') or 'Jellyfin'}",
                f"--start={info['pos'] / 1e7:.1f}"]

        # Title metadata for the overlay (pifin-osd.lua reads pifin-osd-* opts).
        # mpv splits --script-opts on commas, so strip/replace them.
        so = {"series": info.get("series"), "name": info.get("name"), "year": info.get("year"),
              "season": info.get("season"), "episode": info.get("episode")}
        optstr = ",".join(f"pifin-osd-{k}={str(v).replace(',', ' ')}"
                          for k, v in so.items() if v not in (None, ""))
        if optstr:
            args.append(f"--script-opts={optstr}")

        aidx = info.get("aidx")
        apos = next((n for n, s in enumerate(audio) if s["i"] == aidx), None)
        if apos is not None:
            args.append(f"--aid={apos + 1}")

        for s in subs_ext:
            fmt = SUB_FMT.get((s.get("codec") or "").lower(), "srt")
            url = (f"{server}/Videos/{item}/{msid}/Subtitles/{s['i']}/0/Stream.{fmt}"
                   f"?api_key={urllib.parse.quote(token)}")
            args.append(f"--sub-file={url}")
        sidx = info.get("sidx")
        if sidx is None or sidx < 0:
            args.append("--sid=no")
        else:
            ipos = next((n for n, s in enumerate(subs_int) if s["i"] == sidx), None)
            epos = next((n for n, s in enumerate(subs_ext) if s["i"] == sidx), None)
            if ipos is not None:
                args.append(f"--sid={ipos + 1}")
            elif epos is not None:
                args.append(f"--sid={len(subs_int) + epos + 1}")

        url = (f"{server}/Videos/{item}/stream?static=true&mediaSourceId={msid}"
               f"&api_key={urllib.parse.quote(token)}")
        env = ["XDG_RUNTIME_DIR=/run/user/1000", "WAYLAND_DISPLAY=wayland-0"]
        with self.lock:
            self.info = info
            self.proc = subprocess.Popen(["runuser", "-u", os.environ.get("PIFIN_USER", "pifin"), "--", "env", *env, "mpv", *args, url],
                                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # ------------------------------------------------------------- status --
    def status(self):
        i = self.info or {}
        pos = self.get("time-pos")
        dur = self.get("duration")
        return {"loggedIn": True, "playing": True, "external": True, "id": i.get("id"),
                "name": i.get("name"), "series": i.get("series"), "season": i.get("season"),
                "episode": i.get("episode"), "year": i.get("year"),
                "runTicks": int((dur or i.get("runTicks", 0) / 1e7) * 1e7),
                "posTicks": int((pos or 0) * 1e7), "paused": bool(self.get("pause")),
                "muted": bool(self.get("mute")), "hasImage": True, "imageId": i.get("imageId") or i.get("id"),
                "tracks": self.tracks()}

    def tracks(self):
        out = {"audio": [], "sub": []}
        for t in self.get("track-list") or []:
            if t.get("type") not in out:
                continue
            label = " · ".join(x for x in [
                (t.get("lang") or "").upper() or None, t.get("title"),
                (t.get("codec") or "").replace("subrip", "srt").replace("hdmv_pgs_subtitle", "pgs"),
                "external" if t.get("external") else None,
                f"{t.get('demux-channel-count')}ch" if t.get("type") == "audio" and t.get("demux-channel-count") else None,
            ] if x) or f"Track {t.get('id')}"
            out[t["type"]].append({"id": t.get("id"), "label": label, "selected": bool(t.get("selected"))})
        return out

    # ------------------------------------------------------------ actions --
    def action(self, action, data):
        if action in ("playpause", "ok"):
            self.cmd("cycle", "pause")
        elif action in ("stop", "back", "home"):
            self.cmd("quit")
        elif action == "seek":
            self.cmd("seek", int(data["ticks"]) / 1e7, "absolute")
        elif action == "left":
            self.cmd("seek", -10)
        elif action == "right":
            self.cmd("seek", 30)
        elif action == "up":
            self.cmd("cycle", "audio")
        elif action == "down":
            self.cmd("cycle", "sub")
        elif action == "osd":
            self.cmd("show-progress")
        elif action in ("sid", "aid"):
            v = data.get("id")
            self.cmd("set", action, "no" if v in (None, "no", 0, "0") else str(v))
            msg = ("Subtitles: ${current-tracks/sub/lang:off} ${current-tracks/sub/title:}" if action == "sid"
                   else "Audio: ${current-tracks/audio/lang:} ${current-tracks/audio/title:}")
            self.cmd("show-text", msg, 2500)
        elif action == "volup":
            self.cmd("add", "volume", 5)
        elif action == "voldown":
            self.cmd("add", "volume", -5)
        elif action == "mute":
            self.cmd("cycle", "mute")
        else:
            return "ignored"
        return "ok"


# Gathers what the Jellyfin UI chose when Play was pressed (runs in the page).
JS_PLAYBACK_INFO = """
(async () => {
  const a = window.ApiClient;
  if (!a || !a.accessToken()) return null;
  const s = (await a.getSessions({})).find(x => x.DeviceId === a.deviceId());
  if (!s || !s.NowPlayingItem) return null;
  const ps = s.PlayState || {};
  const it = await a.getItem(a.getCurrentUserId(), s.NowPlayingItem.Id);
  const ms = (it.MediaSources || []).find(m => m.Id === ps.MediaSourceId) || (it.MediaSources || [])[0];
  if (!ms) return null;
  return {
    sessionId: s.Id, id: it.Id, msid: ms.Id, name: it.Name, series: it.SeriesName || null,
    season: it.ParentIndexNumber ?? null, episode: it.IndexNumber ?? null, year: it.ProductionYear || null,
    title: it.SeriesName ? `${it.SeriesName} - S${it.ParentIndexNumber}E${it.IndexNumber} - ${it.Name}` : it.Name,
    imageId: (it.ImageTags && it.ImageTags.Primary) ? it.Id : (it.SeriesId || it.Id),
    runTicks: it.RunTimeTicks || 0,
    streams: (ms.MediaStreams || []).map(x => ({i: x.Index, t: x.Type, ext: !!x.IsExternal, codec: x.Codec})),
    aidx: ps.AudioStreamIndex ?? ms.DefaultAudioStreamIndex ?? null,
    sidx: ps.SubtitleStreamIndex ?? ms.DefaultSubtitleStreamIndex ?? -1,
    pos: ps.PositionTicks || 0,
    server: a.serverAddress(), token: a.accessToken(),
  };
})()
"""


def js_save_position(info, pos_ticks):
    """Store the resume position (or mark watched past 90%) in Jellyfin."""
    item = json.dumps(info["id"])
    run = int(info.get("runTicks") or 0)
    pos = int(pos_ticks)
    watched = "true" if run and pos > 0.9 * run else "false"
    return f"""(async () => {{
      const a = window.ApiClient, uid = a.getCurrentUserId(), id = {item};
      const h = {{"Content-Type": "application/json", "Authorization": `MediaBrowser Token="${{a.accessToken()}}"`}};
      if ({watched}) {{
        await fetch(a.getUrl("UserPlayedItems/" + id, {{userId: uid}}), {{method: "POST", headers: h}});
        await fetch(a.getUrl("UserItems/" + id + "/UserData", {{userId: uid}}), {{method: "POST", headers: h, body: JSON.stringify({{PlaybackPositionTicks: 0}})}});
      }} else {{
        await fetch(a.getUrl("UserItems/" + id + "/UserData", {{userId: uid}}), {{method: "POST", headers: h, body: JSON.stringify({{PlaybackPositionTicks: {pos}}})}});
      }}
      return "ok"; }})()"""


def js_report(kind, info, pos_ticks, paused=False):
    """Report playback to Jellyfin through the web client (keeps Continue Watching right)."""
    body = json.dumps({"ItemId": info["id"], "MediaSourceId": info["msid"], "PositionTicks": int(pos_ticks),
                       "IsPaused": bool(paused), "CanSeek": True, "PlayMethod": "DirectPlay"})
    fn = {"start": "reportPlaybackStart", "progress": "reportPlaybackProgress", "stop": "reportPlaybackStopped"}[kind]
    return f"(async () => {{ await window.ApiClient.{fn}({body}); return 'ok'; }})()"
