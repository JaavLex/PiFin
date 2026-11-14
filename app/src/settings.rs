//! Settings plumbing: the app reads /var/lib/pifin/settings.json directly (cheap stat,
//! re-parse on mtime change) and changes it only through the root daemon's socket
//! (one JSON object per line, see files/pifin-settingsd).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

pub struct Paths {
    pub settings: String,
    pub sock: String,
    pub wifi: String,
    pub pair_token: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

pub fn paths() -> Paths {
    Paths {
        settings: env_or("PIFIN_SETTINGS", "/var/lib/pifin/settings.json"),
        sock: env_or("PIFIN_SETTINGS_SOCK", "/run/pifin/settings.sock"),
        wifi: env_or("PIFIN_WIFI_FILE", "/run/pifin/wifi.json"),
        pair_token: env_or("PIFIN_PAIR_TOKEN", "/var/lib/pifin/pair-token"),
    }
}

/// Playback limits. `max_bitrate` 0 = no limit.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Playback {
    pub max_bitrate: i64,
    pub max_height: i32,
    pub hdr: bool,
    pub prefer_original: bool,
}

// Not derived: a partial file must fall back to 1080p / prefer original, not to zeros.
impl Default for Playback {
    fn default() -> Self {
        Playback { max_bitrate: 0, max_height: 1080, hdr: false, prefer_original: true }
    }
}

/// Per-network overrides: a missing key means "use the global value".
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NetLimits {
    pub max_bitrate: Option<i64>,
    pub max_height: Option<i32>,
    pub hdr: Option<bool>,
    pub prefer_original: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Display {
    pub output_max_height: i32,
    pub subtitle_scale: f32,
    pub audio_passthrough: bool,
}

impl Default for Display {
    fn default() -> Self {
        Display { output_max_height: 1080, subtitle_scale: 1.0, audio_passthrough: false }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct System {
    pub jellyfin_url: String,
    pub hotspot_ssid: String,
    pub pin_set: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub playback: Playback,
    pub networks: BTreeMap<String, NetLimits>,
    pub display: Display,
    pub system: System,
}

/// Last parse: (mtime of the file it came from, value). `None` mtime = file missing.
type Cache = Option<(Option<(SystemTime, u64)>, Arc<Settings>)>;
static CACHE: Mutex<Cache> = Mutex::new(None);

/// The settings, re-read only when the file's mtime changed. A missing file gives the
/// defaults; a file that does not parse keeps the last good value (or the defaults).
pub fn current() -> Arc<Settings> {
    load_cached(&CACHE, &paths().settings)
}

fn load_cached(cache: &Mutex<Cache>, path: &str) -> Arc<Settings> {
    // mtime plus length: two writes within the timestamp granularity rarely keep both
    let mtime = std::fs::metadata(path).and_then(|m| Ok((m.modified()?, m.len()))).ok();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((seen, value)) = guard.as_ref() {
        if *seen == mtime {
            return value.clone();
        }
    }
    let fresh = match mtime {
        None => Arc::new(Settings::default()),
        Some(_) => match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Settings>(&t).ok()) {
            Some(s) => Arc::new(s),
            None => guard.as_ref().map(|(_, v)| v.clone()).unwrap_or_default(),
        },
    };
    *guard = Some((mtime, fresh.clone()));
    fresh
}

/// SSID of the network the Pi is connected to (wifi.json), if any.
pub fn current_ssid() -> Option<String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct W {
        state: String,
        ssid: String,
    }
    let w: W = serde_json::from_str(&std::fs::read_to_string(paths().wifi).ok()?).ok()?;
    (w.state == "CONNECTED" && !w.ssid.is_empty()).then_some(w.ssid)
}

/// Global playback limits with the network's own overrides on top.
pub fn effective(s: &Settings, ssid: Option<&str>) -> Playback {
    let mut p = s.playback.clone();
    if let Some(n) = ssid.and_then(|id| s.networks.get(id)) {
        if let Some(v) = n.max_bitrate { p.max_bitrate = v; }
        if let Some(v) = n.max_height { p.max_height = v; }
        if let Some(v) = n.hdr { p.hdr = v; }
        if let Some(v) = n.prefer_original { p.prefer_original = v; }
    }
    p
}

/// The pairing token the phone page needs: 32 hex chars, else None.
pub fn pair_token() -> Option<String> {
    let t = std::fs::read_to_string(paths().pair_token).ok()?;
    let t = t.trim();
    (t.len() == 32 && t.chars().all(|c| c.is_ascii_hexdigit())).then(|| t.to_string())
}

#[derive(Clone, Debug)]
pub struct DaemonError {
    pub code: String,
    pub message: String,
}

fn unreachable_err(what: &str) -> DaemonError {
    DaemonError { code: "unreachable".into(), message: format!("Settings service not reachable ({what}).") }
}

/// One request to the settings daemon: `{"op": op, ..args}` out, the `result` back.
/// Timeouts callers use: 5 s; `wifi_scan` 20 s; `tailscale_login` 40 s.
pub fn request(op: &str, args: serde_json::Value, timeout: Duration) -> Result<serde_json::Value, DaemonError> {
    request_at(&paths().sock, op, args, timeout)
}

fn request_at(sock: &str, op: &str, args: serde_json::Value, timeout: Duration) -> Result<serde_json::Value, DaemonError> {
    let mut obj = match args {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("op".into(), op.into());
    let mut s = UnixStream::connect(sock).map_err(|e| unreachable_err(&e.kind().to_string()))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    s.write_all(format!("{}\n", serde_json::Value::Object(obj)).as_bytes()).map_err(|e| unreachable_err(&e.kind().to_string()))?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| unreachable_err(&e.kind().to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&line).map_err(|_| unreachable_err("no answer"))?;
    if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
        return Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null));
    }
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    Err(DaemonError { code: if text("error").is_empty() { "failed".into() } else { text("error") }, message: text("message") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    const SPEC_EXAMPLE: &str = r#"{"version": 1,
      "playback": {"max_bitrate": 0, "max_height": 1080, "hdr": false, "prefer_original": true},
      "networks": {"Hotel": {"max_bitrate": 6000000, "max_height": 720}},
      "display":  {"output_max_height": 1080, "subtitle_scale": 1.0, "audio_passthrough": false},
      "system":   {"jellyfin_url": "http://host:8096", "hotspot_ssid": "PiFin-setup", "pin_set": false}}"#;

    fn tmp(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("pifin-settings-test-{}-{name}", std::process::id()));
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn effective_merges_a_partial_network_entry_over_global() {
        let mut s = Settings::default();
        s.playback.max_bitrate = 20_000_000;
        s.playback.max_height = 2160;
        s.networks.insert("Hotel".into(), NetLimits { max_bitrate: Some(6_000_000), prefer_original: Some(false), ..Default::default() });
        let p = effective(&s, Some("Hotel"));
        assert_eq!((p.max_bitrate, p.max_height, p.hdr, p.prefer_original), (6_000_000, 2160, false, false));
        let p = effective(&s, Some("Elsewhere"));
        assert_eq!((p.max_bitrate, p.prefer_original), (20_000_000, true));
        assert_eq!(effective(&s, None).max_bitrate, 20_000_000);
    }

    #[test]
    fn spec_example_parses_and_a_missing_file_gives_defaults() {
        let s: Settings = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(s.networks["Hotel"].max_height, Some(720));
        assert_eq!(s.system.jellyfin_url, "http://host:8096");
        assert!(s.playback.prefer_original && s.display.subtitle_scale == 1.0);
        let missing = tmp("missing");
        let d = load_cached(&Mutex::new(None), &missing);
        assert!(d.playback.prefer_original);
        assert_eq!((d.playback.max_height, d.display.output_max_height), (1080, 1080));
        // a partial file keeps the other defaults
        let p: Settings = serde_json::from_str(r#"{"playback": {"max_bitrate": 5}}"#).unwrap();
        assert!(p.playback.prefer_original && p.playback.max_height == 1080);
    }

    #[test]
    fn reload_on_change_and_keep_last_good_on_a_parse_error() {
        let path = tmp("reload");
        let cache = Mutex::new(None);
        std::fs::write(&path, SPEC_EXAMPLE).unwrap();
        assert_eq!(load_cached(&cache, &path).system.hotspot_ssid, "PiFin-setup");
        // new content with a different mtime is picked up
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, SPEC_EXAMPLE.replace("PiFin-setup", "Other")).unwrap();
        assert_eq!(load_cached(&cache, &path).system.hotspot_ssid, "Other");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load_cached(&cache, &path).system.hotspot_ssid, "Other");
        std::fs::remove_file(&path).ok();
    }

    fn serve(name: &str, reply: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let sock = tmp(name);
        let _ = std::fs::remove_file(&sock);
        let l = UnixListener::bind(&sock).unwrap();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(c.try_clone().unwrap()).read_line(&mut line).unwrap();
            c.write_all(reply.as_bytes()).unwrap();
            line
        });
        (sock, h)
    }

    #[test]
    fn request_maps_results_and_errors() {
        let t = Duration::from_secs(2);
        let (sock, h) = serve("ok", "{\"ok\": true, \"result\": {\"n\": 1}}\n");
        let r = request_at(&sock, "get", serde_json::json!({"x": 2}), t).unwrap();
        assert_eq!(r["n"], 1);
        let sent: serde_json::Value = serde_json::from_str(&h.join().unwrap()).unwrap();
        assert_eq!((sent["op"].as_str(), sent["x"].as_i64()), (Some("get"), Some(2)));

        let (sock, h) = serve("err", "{\"ok\": false, \"error\": \"pin_wrong\", \"message\": \"Wrong PIN.\"}\n");
        let e = request_at(&sock, "set", serde_json::json!({}), t).unwrap_err();
        assert_eq!((e.code.as_str(), e.message.as_str()), ("pin_wrong", "Wrong PIN."));
        h.join().unwrap();

        let e = request_at(&tmp("nobody-listens"), "get", serde_json::json!({}), t).unwrap_err();
        assert_eq!(e.code, "unreachable");
    }

    #[test]
    fn pair_token_needs_32_hex_chars() {
        let path = tmp("token");
        std::env::set_var("PIFIN_PAIR_TOKEN", &path);
        std::fs::write(&path, "0123456789abcdef0123456789ABCDEF\n").unwrap();
        assert_eq!(pair_token().as_deref(), Some("0123456789abcdef0123456789ABCDEF"));
        std::fs::write(&path, "xyz").unwrap();
        assert_eq!(pair_token(), None);
        std::fs::remove_file(&path).ok();
        std::env::remove_var("PIFIN_PAIR_TOKEN");
    }
}
