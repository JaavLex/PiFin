// tools/shot-remote.mjs — render the phone remote with mocked /api/* into docs/screenshots/.
// Fictional titles and flat-colour posters only: never point this at a real library.
//
//   PLAYWRIGHT=/path/to/playwright node tools/shot-remote.mjs
//   (or: npm i -g playwright && node tools/shot-remote.mjs)
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "..");
const REMOTE = path.join(ROOT, "files", "pifin-remote");
const OUT = path.join(ROOT, "docs", "screenshots");
const INDEX = fs.readFileSync(path.join(REMOTE, "index.html"));
const ICON = fs.readFileSync(path.join(REMOTE, "icon.svg"));

const SHOW = "The Last Lighthouse";   // invented show
const EPISODE = "Fog and Glass";
const TICK = 1e7;                     // 1 second in Jellyfin ticks

const poster = (a, b, line1, line2) => `<svg xmlns="http://www.w3.org/2000/svg" width="320" height="480">
  <defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
    <stop offset="0" stop-color="${a}"/><stop offset="1" stop-color="${b}"/></linearGradient></defs>
  <rect width="320" height="480" fill="url(#g)"/>
  <circle cx="160" cy="150" r="54" fill="rgba(255,255,255,.18)"/>
  <text x="24" y="420" font-family="sans-serif" font-size="26" font-weight="700" fill="#fff">${line1}</text>
  <text x="24" y="452" font-family="sans-serif" font-size="20" fill="rgba(255,255,255,.85)">${line2}</text>
</svg>`;

const IDLE = { ok: true, loggedIn: true, playing: false };
const PLAYING = {
  ok: true, loggedIn: true, playing: true, external: true, type: "Episode",
  id: "show-1", imageId: "lighthouse", hasImage: true,
  name: EPISODE, series: SHOW, season: 2, episode: 4, year: null,
  runTicks: 2730 * TICK, posTicks: 987 * TICK, paused: false, muted: false, volume: 0.8,
  tracks: {
    audio: [{ id: 1, label: "ENG · eac3 · 5.1ch", selected: true },
            { id: 2, label: "FRE · aac · 2.0ch", selected: false }],
    sub: [{ id: 1, label: "ENG · srt", selected: false },
          { id: 2, label: "FRE · ass", selected: true }],
  },
};

let mode = "idle";

const server = http.createServer((req, res) => {
  const url = req.url.split("?")[0];
  const send = (body, type = "application/json", code = 200) => {
    res.writeHead(code, { "Content-Type": type, "Cache-Control": "no-store" });
    res.end(body);
  };
  if (req.method === "POST") return send(JSON.stringify({ ok: true, result: "ok" }));
  if (url === "/" || url === "/index.html") return send(INDEX, "text/html; charset=utf-8");
  if (url === "/api/status") return send(JSON.stringify(mode === "idle" ? IDLE : PLAYING));
  if (url.startsWith("/api/art/")) {
    return send(poster("#2b3f6b", "#0f766e", SHOW, `S2:E4  ${EPISODE}`), "image/svg+xml");
  }
  if (url === "/manifest.webmanifest") {
    return send(JSON.stringify({ name: "PiFin Remote", short_name: "Remote", start_url: "/",
      display: "standalone", background_color: "#0b0b12", theme_color: "#0b0b12" }));
  }
  if (url === "/icon.svg") return send(ICON, "image/svg+xml");
  send("{}", "application/json", 404);
});

const require = createRequire(import.meta.url);
let chromium;
try {
  ({ chromium } = require(process.env.PLAYWRIGHT || "playwright"));
} catch {
  console.error("Playwright not found. Install it, or point PLAYWRIGHT at its package dir.");
  process.exit(2);
}

await fs.promises.mkdir(OUT, { recursive: true });
await new Promise(r => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch();

for (const shot of ["idle", "playing"]) {
  mode = shot;
  const ctx = await browser.newContext({
    viewport: { width: 390, height: 844 },   // a mid-size phone
    deviceScaleFactor: 2, isMobile: true, hasTouch: true, reducedMotion: "reduce",
  });
  const page = await ctx.newPage();
  await page.goto(base, { waitUntil: "load" });
  await page.waitForSelector("#dot.on");     // first /api/status came back
  if (shot === "playing") {
    await page.waitForSelector("#np.show");
    await page.waitForFunction(() => {
      const img = document.getElementById("art");
      return img && img.complete && img.naturalWidth > 0;
    });
  }
  await page.waitForTimeout(250);            // let the last paint settle
  const file = path.join(OUT, `remote-${shot}.png`);
  await page.screenshot({ path: file, fullPage: false });
  console.log("wrote", path.relative(ROOT, file));
  await ctx.close();
}

await browser.close();
server.close();
