// The phone's first-setup wizard logic (files/pifin-remote/settings.html, between the
// "wizard logic" markers). Run: node tools/test_wizard.mjs
import { readFileSync } from "node:fs";
import assert from "node:assert/strict";

const html = readFileSync(new URL("../files/pifin-remote/settings.html", import.meta.url), "utf8");
const start = html.indexOf("// ---- wizard logic");
const end = html.indexOf("// ---- end wizard logic ----");
assert.ok(start > 0 && end > start, "wizard logic markers");
const W = new Function(html.slice(start, end) +
  "\nreturn { WIZ_STEPS, wizVisible, wizCheck, wizNext, wizBack, wizFinal };")();

const fresh = { system: { jellyfin_url: "" } }, done = { system: { jellyfin_url: "http://h:8096" } };
// shown on a fresh PiFin, hidden once configured, kept for its last step
assert.equal(W.wizVisible(fresh, "server"), true);
assert.equal(W.wizVisible(done, "server"), false);
assert.equal(W.wizVisible(done, "done"), true);
assert.equal(W.wizVisible(null, "server"), false);

// the server address is required and checked
assert.deepEqual(W.wizNext("server", { url: "" }), { step: "server", error: "The address must look like http://host:8096." });
assert.equal(W.wizNext("server", { url: "jellyfin.local" }).step, "server");
assert.deepEqual(W.wizNext("server", { url: " http://192.0.2.5:8096 " }), { step: "tailscale", error: "" });
// Tailscale is optional, but a key must look like one
assert.equal(W.wizNext("tailscale", { tsKey: "" }).step, "hotspot");
assert.equal(W.wizNext("tailscale", { tsKey: "hello" }).step, "tailscale");
assert.equal(W.wizNext("tailscale", { tsKey: "tskey-auth-abcdef123456" }).step, "hotspot");
// hotspot password: empty keeps the current one; else 8 to 63
assert.equal(W.wizNext("hotspot", { hotspot: "" }).step, "pin");
assert.equal(W.wizNext("hotspot", { hotspot: "short" }).step, "hotspot");
assert.equal(W.wizNext("hotspot", { hotspot: "x".repeat(64) }).step, "hotspot");
assert.equal(W.wizNext("hotspot", { hotspot: "long enough" }).step, "pin");
// PIN optional; 4-8 digits, repeated
assert.equal(W.wizNext("pin", { pin: "", pin2: "" }).step, "done");
assert.equal(W.wizNext("pin", { pin: "12", pin2: "12" }).step, "pin");
assert.equal(W.wizNext("pin", { pin: "1234", pin2: "1235" }).error, "The PINs don't match.");
assert.equal(W.wizNext("pin", { pin: "", pin2: "1234" }).step, "pin");
assert.equal(W.wizNext("pin", { pin: "1234", pin2: "1234" }).step, "done");
assert.equal(W.wizBack("tailscale"), "server");
assert.equal(W.wizBack("server"), "server");

// Finish: one set with the server (and the hotspot password when given), then the PIN
assert.deepEqual(W.wizFinal({ url: " http://h:8096 ", hotspot: "", pin: "" }),
  { changes: { "system.jellyfin_url": "http://h:8096" }, pin: null });
assert.deepEqual(W.wizFinal({ url: "http://h:8096", hotspot: "long enough", pin: "1234" }),
  { changes: { "system.jellyfin_url": "http://h:8096", "secrets.hotspot_pass": "long enough" }, pin: "1234" });
console.log("wizard tests passed");
