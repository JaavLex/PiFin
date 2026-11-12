#!/usr/bin/env python3
"""Mock Jellyfin server with a demo library, for README screenshots.

The README must never show anyone's own library, so the TV shots are taken
against this server: eight Blender open movies and the Caminandes series
(artwork from Wikimedia Commons, CC BY / CC0, credited in the README), three
users with their own Continue Watching, and Quick Connect that never approves.
It answers only the calls the PiFin app makes. Stdlib only.

  tools/demo-server.py --fetch-art ART_DIR     # once: download the artwork
  tools/demo-server.py ART_DIR [--port 8096]   # serve
  tools/demo-server.py --auth http://HOST:8096 > demo-auth.json   # app login file
"""
import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

T = 10_000_000  # ticks per second

USERS = {  # token: (user id, name)
    "demo-alex": ("u1", "Alex"),
    "demo-sam": ("u2", "Sam"),
    "demo-kids": ("u3", "Kids"),
}

# artwork: local name -> Commons file (the width the thumbnail is fetched at)
ART = {
    "bbb-p": ("File:Big buck bunny poster big.jpg", 500), "bbb-b": ("File:Big Buck Bunny 4K.webm", 1280),
    "sintel-p": ("File:Sintel poster.jpg", 500), "sintel-b": ("File:Sintel-screenshot-1.jpg", 1280),
    "tos-p": ("File:Tos-poster.png", 500), "tos-b": ("File:Tears of Steel 1080p.webm", 1280),
    "ed-p": ("File:ElephantsDreamPoster.jpg", 500), "ed-b": ("File:Elephants Dream - Emo and Proog.png", 1280),
    "cosmos-p": ("File:CosmosLaundromatPoster.jpg", 500), "cosmos-b": ("File:CosmosLaundromatYouTubeThumbnail.jpg", 1280),
    "spring-p": ("File:Spring2019AlphaPosterBlender.jpg", 500),
    "sprite-p": ("File:Sprite Fright-movie poster.jpg", 500),
    "charge-p": ("File:Charge-movie poster.jpg", 500),
    "cam-e1": ("File:Caminandes- Llama Drama - Short Movie.ogv", 640),
    "cam-e2": ("File:Blender Foundation - Caminandes - Episode 2 - Gran Dillama - Cover thumbnail.png", 640),
    "cam-e3": ("File:Blender Foundation - Caminandes - Episode 3 - Llamigos - Cover thumbnail.png", 640),
    "cam-b": ("File:Blender Foundation - Caminandes - Episode 3 - Llamigos - Koro and Oti share more food to eat at night.png", 1280),
}


def movie(id, name, year, secs, overview, genres, rating, art_b=True, people=()):
    return {"Id": id, "Name": name, "Type": "Movie", "ProductionYear": year, "RunTimeTicks": secs * T,
            "Overview": overview, "Genres": genres, "CommunityRating": rating, "OfficialRating": "PG",
            "ImageTags": {"Primary": id + "-p"}, "BackdropImageTags": [id + "-b"] if art_b else [id + "-p"],
            "People": [{"Id": "p-" + re.sub(r"\W", "", n).lower(), "Name": n, "Role": r, "Type": "Actor"} for n, r in people],
            "MediaSources": [{"Id": id, "Bitrate": 8_000_000, "MediaStreams": [
                {"Index": 0, "Type": "Video", "Codec": "h264", "DisplayTitle": "1080p H264 SDR"},
                {"Index": 1, "Type": "Audio", "Codec": "aac", "DisplayTitle": "English - AAC - Stereo - Default", "IsDefault": True},
                {"Index": 2, "Type": "Subtitle", "Codec": "subrip", "DisplayTitle": "English - SUBRIP", "IsTextSubtitleStream": True}]}]}


MOVIES = [
    movie("charge", "Charge", 2022, 236, "An old man raids a heavily guarded energy plant in a world that is running out of power.", ["Action", "Science Fiction"], 7.1, art_b=False),
    movie("sprite", "Sprite Fright", 2021, 629, "Rowdy teenagers on a trip to the woods meet the small mushroom folk who live there, and learn some manners.", ["Comedy", "Horror"], 7.3, art_b=False),
    movie("spring", "Spring", 2019, 464, "A shepherd girl and her dog face ancient spirits to keep the cycle of life going.", ["Fantasy", "Animation"], 7.4, art_b=False),
    movie("cosmos", "Cosmos Laundromat", 2015, 730, "On a desolate island, a sheep named Franck meets a mysterious salesman who offers him a whole new life.", ["Animation", "Fantasy"], 7.0),
    movie("tos", "Tears of Steel", 2012, 734, "In a future Amsterdam, a group of scientists tries to save the world from destructive robots.", ["Science Fiction", "Action"], 6.2,
          people=[("Derek de Lint", ""), ("Sergio Hasselbaink", ""), ("Rogier Schippers", ""), ("Vanja Rukavina", ""), ("Denise Rebergen", "")]),
    movie("sintel", "Sintel", 2010, 888, "A lonely young woman searches the world for the baby dragon she once rescued and raised.", ["Fantasy", "Adventure"], 7.4,
          people=[("Halina Reijn", "Sintel"), ("Thom Hoffman", "Shaman")]),
    movie("bbb", "Big Buck Bunny", 2008, 596, "A giant rabbit with a heart bigger than himself gets even with three bullying rodents.", ["Animation", "Comedy"], 6.5),
    movie("ed", "Elephants Dream", 2006, 654, "Two men wander through a strange machine world, and do not agree on what it is.", ["Animation", "Science Fiction"], 5.9,
          people=[("Tygo Gernandt", "Proog"), ("Cas Jansen", "Emo")]),
]
SERIES = {"Id": "cam", "Name": "Caminandes", "Type": "Series", "ProductionYear": 2013, "Genres": ["Animation", "Comedy"],
          "Overview": "Koro the llama keeps finding new ways to reach the grass on the other side, in the wilds of Patagonia.",
          "CommunityRating": 7.6, "ImageTags": {"Primary": "cam-e2"}, "BackdropImageTags": ["cam-b"]}
SEASON = {"Id": "cam-s1", "Name": "Season 1", "Type": "Season", "IndexNumber": 1, "SeriesId": "cam", "SeriesName": "Caminandes",
          "ImageTags": {"Primary": "cam-e3"}}


def episode(n, name, year, secs, overview):
    return {"Id": f"cam-e{n}", "Name": name, "Type": "Episode", "SeriesName": "Caminandes", "SeriesId": "cam",
            "SeasonId": "cam-s1", "SeasonName": "Season 1", "ParentIndexNumber": 1, "IndexNumber": n,
            "ProductionYear": year, "RunTimeTicks": secs * T, "Overview": overview,
            "ImageTags": {"Primary": f"cam-e{n}"}, "BackdropImageTags": [], "MediaSources": MOVIES[0]["MediaSources"]}


EPISODES = [
    episode(1, "Llama Drama", 2013, 90, "Koro the llama wants to cross a road. It does not go well."),
    episode(2, "Gran Dillama", 2013, 146, "The grass is greener on the other side of the electric fence."),
    episode(3, "Llamigos", 2016, 150, "Winter. Koro and Oti the penguin fight over the last red berries."),
]
ITEMS = {i["Id"]: i for i in MOVIES + EPISODES + [SERIES, SEASON]}
VIEWS = [{"Id": "lib-movies", "Name": "Movies", "Type": "CollectionFolder", "CollectionType": "movies"},
         {"Id": "lib-shows", "Name": "Shows", "Type": "CollectionFolder", "CollectionType": "tvshows"}]

# per user: (item id, watched fraction) in progress, and next up episode
STATE = {
    "u1": {"resume": [("sintel", 0.42), ("tos", 0.67), ("cam-e2", 0.3)], "next": ["cam-e2"]},
    "u2": {"resume": [("cosmos", 0.2)], "next": ["cam-e3"]},
    "u3": {"resume": [("bbb", 0.55)], "next": ["cam-e1"]},
}


def with_user(item, uid):
    it = dict(item)
    frac = dict(STATE.get(uid, {}).get("resume", [])).get(it["Id"])
    run = it.get("RunTimeTicks", 0)
    it["UserData"] = {"PlaybackPositionTicks": int(run * frac) if frac else 0, "Played": False,
                      "PlayedPercentage": frac * 100 if frac else None}
    return it


def items(lst, uid):
    return {"Items": [with_user(i, uid) for i in lst], "TotalRecordCount": len(lst)}


class Handler(BaseHTTPRequestHandler):
    art_dir = "."

    def log_message(self, fmt, *args):
        sys.stderr.write("%s\n" % (fmt % args))

    def send(self, code, body=b"", ctype="application/json"):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def user(self):
        m = re.search(r'Token="([^"]*)"', self.headers.get("Authorization", ""))
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        token = m.group(1) if m else q.get("api_key", [""])[0]
        return USERS.get(token, (None, None))

    def do_POST(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/QuickConnect/Initiate":
            return self.send(200, {"Secret": "demo", "Code": "318427", "Authenticated": False})
        self.send(204)

    def do_DELETE(self):
        self.send(204)

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        path, q = u.path, {k: v[0] for k, v in urllib.parse.parse_qs(u.query).items()}
        if path == "/System/Info/Public":
            return self.send(200, {"ServerName": "Demo", "Version": "10.10.7", "Id": "demo"})
        if path == "/QuickConnect/Connect":
            return self.send(200, {"Secret": "demo", "Code": "318427", "Authenticated": False})
        m = re.fullmatch(r"/Items/([\w-]+)/Images/(\w+)(?:/\d+)?", path)
        if m:
            item = ITEMS.get(m.group(1), {})
            tag = (item.get("ImageTags", {}).get("Primary") if m.group(2) == "Primary"
                   else (item.get("BackdropImageTags") or [None])[0])
            for ext in (".jpg", ".png"):
                f = os.path.join(self.art_dir, f"{tag}{ext}")
                if tag and os.path.exists(f):
                    return self.send(200, open(f, "rb").read(), "image/png" if ext == ".png" else "image/jpeg")
            return self.send(404, {"error": "no image"})
        uid, name = self.user()
        if not uid:
            return self.send(401, {"error": "unauthorized"})
        if path == "/Users/Me":
            return self.send(200, {"Id": uid, "Name": name, "Policy": {"IsAdministrator": uid == "u1"}})
        if re.fullmatch(r"/Users/\w+/Images/Primary", path):
            return self.send(404, {"error": "no avatar"})
        if path.endswith("/Items/Resume"):
            st = STATE[uid]["resume"]
            return self.send(200, items([ITEMS[i] for i, _ in st], uid))
        if path == "/Shows/NextUp":
            return self.send(200, items([ITEMS[i] for i in STATE[uid]["next"]], uid))
        if path.endswith("/Views"):
            return self.send(200, {"Items": VIEWS, "TotalRecordCount": len(VIEWS)})
        if path.endswith("/Items/Latest"):
            lst = MOVIES if q.get("ParentId") == "lib-movies" else [SERIES]
            return self.send(200, [with_user(i, uid) for i in lst])
        if m := re.fullmatch(r"/Shows/([\w-]+)/Seasons", path):
            return self.send(200, items([SEASON], uid))
        if m := re.fullmatch(r"/Shows/([\w-]+)/Episodes", path):
            return self.send(200, items(EPISODES, uid))
        if m := re.fullmatch(r"/Users/\w+/Items/([\w-]+)", path):
            if m.group(1) in ITEMS:
                return self.send(200, with_user(ITEMS[m.group(1)], uid))
            return self.send(404, {"error": "not found"})
        if re.fullmatch(r"/Users/\w+/Items", path):
            types = q.get("IncludeItemTypes", "Movie,Series,Episode").split(",")
            pool = [i for i in MOVIES + [SERIES] + EPISODES if i["Type"] in types]
            if term := q.get("SearchTerm", "").lower():
                pool = [i for i in pool if term in i["Name"].lower() or term in i.get("SeriesName", "").lower()]
            elif q.get("ParentId") == "lib-movies":
                pool = [i for i in pool if i["Type"] == "Movie"]
            elif q.get("ParentId") == "lib-shows":
                pool = [i for i in pool if i["Type"] == "Series"]
            if q.get("SortBy") == "SortName":
                pool.sort(key=lambda i: i["Name"])
            start, limit = int(q.get("StartIndex", 0)), int(q.get("Limit", 100))
            res = items(pool[start:start + limit], uid)
            res["TotalRecordCount"] = len(pool)
            return self.send(200, res)
        self.send(404, {"error": "not mocked: " + path})


def fetch_art(out):
    """Download the artwork (Commons thumbnails) and print each file's license."""
    ua = {"User-Agent": "PiFin-demo/0.1 (github.com/JaavLex/PiFin)"}
    os.makedirs(out, exist_ok=True)
    for name, (title, width) in ART.items():
        if any(os.path.exists(os.path.join(out, name + e)) for e in (".jpg", ".png")):
            continue
        qs = urllib.parse.urlencode({"action": "query", "prop": "imageinfo", "iiprop": "url|extmetadata",
                                     "iiurlwidth": width, "format": "json", "titles": title})
        for attempt in range(6):
            try:
                d = json.load(urllib.request.urlopen(urllib.request.Request(
                    "https://commons.wikimedia.org/w/api.php?" + qs, headers=ua)))
                ii = next(iter(d["query"]["pages"].values()))["imageinfo"][0]
                url = ii["thumburl"]
                data = urllib.request.urlopen(urllib.request.Request(url, headers=ua)).read()
                break
            except urllib.error.HTTPError as e:  # 429: Commons rate limit
                if e.code != 429:
                    raise
                time.sleep(10 * (attempt + 1))
        ext = ".png" if url.split("?")[0].endswith(".png") else ".jpg"
        open(os.path.join(out, name + ext), "wb").write(data)
        print(name, ii["extmetadata"].get("LicenseShortName", {}).get("value"), title)
        time.sleep(3)


def main():
    a = sys.argv[1:]
    if a[:1] == ["--fetch-art"]:
        return fetch_art(a[1])
    if a[:1] == ["--auth"]:
        users = [{"user_id": uid, "name": n, "token": t, "device_id": "demo" + uid} for t, (uid, n) in USERS.items()]
        print(json.dumps({"server": a[1], "user_id": "u1", "token": "demo-alex", "device_id": "demou1", "users": users}, indent=1))
        return
    Handler.art_dir = a[0]
    port = int(a[a.index("--port") + 1]) if "--port" in a else 8096
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
