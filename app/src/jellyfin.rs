//! Minimal blocking Jellyfin client: only the calls the UI needs, small responses.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Auth {
    pub server: String,
    pub user_id: String,
    pub token: String,
    pub device_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaStream {
    pub index: i32,
    #[serde(rename = "Type")]
    pub kind: String,
    pub codec: Option<String>,
    pub display_title: Option<String>,
    pub is_external: bool,
    pub is_default: bool,
    pub is_text_subtitle_stream: bool,
    pub title: Option<String>,
    pub height: Option<i32>,
    pub real_frame_rate: Option<f64>,
    pub average_frame_rate: Option<f64>,
    /// "HDR" or "SDR" (video streams).
    pub video_range: Option<String>,
    /// ISO 639 code of the track's language (display only).
    pub language: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MediaSource {
    pub id: String,
    pub bitrate: Option<i64>,
    pub media_streams: Vec<MediaStream>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    #[serde(rename = "Type")]
    pub kind: String,
    pub primary_image_tag: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserData {
    pub playback_position_ticks: i64,
    pub played: bool,
    pub played_percentage: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Item {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub season_name: Option<String>,
    pub parent_index_number: Option<i32>,
    pub index_number: Option<i32>,
    pub production_year: Option<i32>,
    pub run_time_ticks: Option<i64>,
    pub overview: Option<String>,
    pub collection_type: Option<String>,
    pub image_tags: std::collections::HashMap<String, String>,
    pub backdrop_image_tags: Vec<String>,
    pub user_data: Option<UserData>,
    pub people: Vec<Person>,
    pub media_sources: Vec<MediaSource>,
    pub genres: Vec<String>,
    pub community_rating: Option<f64>,
    pub official_rating: Option<String>,
    /// Set by the app for the action cards on a movie page ("resume" / "start").
    #[serde(skip)]
    pub action: Option<&'static str>,
    /// The server's default (audio, subtitle) stream indexes for this user; None until fetched.
    #[serde(skip)]
    pub defaults: Option<(Option<i32>, Option<i32>)>,
}

impl Item {
    /// "S1:E2 · Name" for episodes, the year for movies.
    pub fn subtitle(&self) -> String {
        match (self.parent_index_number, self.index_number) {
            (Some(s), Some(e)) if self.kind == "Episode" => format!("S{s}:E{e} · {}", self.name),
            _ => self.production_year.map(|y| y.to_string()).unwrap_or_default(),
        }
    }
    pub fn title(&self) -> &str {
        self.series_name.as_deref().filter(|_| self.kind == "Episode").unwrap_or(&self.name)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Items {
    items: Vec<Item>,
    total_record_count: i64,
}
/// Audio language, subtitle language ("" = Any) and subtitle mode of a Jellyfin user.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LangPrefs {
    pub audio: String,
    pub subtitle: String,
    pub mode: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct PbSource {
    default_audio_stream_index: Option<i32>,
    default_subtitle_stream_index: Option<i32>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct PbInfo {
    media_sources: Vec<PbSource>,
}

/// (audio, subtitle) defaults out of a PlaybackInfo answer; subtitle -1 or missing = none.
fn parse_playback_defaults(json: &str) -> Result<(Option<i32>, Option<i32>), String> {
    let p: PbInfo = serde_json::from_str(json).map_err(|e| format!("bad PlaybackInfo: {e}"))?;
    Ok(p.media_sources.first().map_or((None, None), |s| {
        (s.default_audio_stream_index.filter(|i| *i >= 0), s.default_subtitle_stream_index.filter(|i| *i >= 0))
    }))
}

/// Read the three language keys out of a user's Configuration.
pub(crate) fn lang_prefs_of(cfg: &serde_json::Value) -> LangPrefs {
    let s = |k: &str| cfg.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    LangPrefs { audio: s("AudioLanguagePreference"), subtitle: s("SubtitleLanguagePreference"), mode: s("SubtitleMode") }
}

/// Write the three language keys into a Configuration.
fn with_lang_prefs(mut cfg: serde_json::Value, p: &LangPrefs) -> serde_json::Value {
    if let Some(o) = cfg.as_object_mut() {
        o.insert("AudioLanguagePreference".into(), p.audio.clone().into());
        o.insert("SubtitleLanguagePreference".into(), p.subtitle.clone().into());
        o.insert("SubtitleMode".into(), p.mode.clone().into());
    }
    cfg
}

/// Cultures list -> (ThreeLetterISOLanguageName, DisplayName), deduped by code, sorted by name.
fn parse_cultures(v: &[serde_json::Value]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for c in v {
        let (Some(code), Some(name)) = (c.get("ThreeLetterISOLanguageName").and_then(|x| x.as_str()), c.get("DisplayName").and_then(|x| x.as_str())) else { continue };
        if !code.is_empty() && !out.iter().any(|(k, _)| k == code) {
            out.push((code.to_string(), name.to_string()));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_defaults_minus_one_and_missing_mean_none() {
        let p = |s: &str| parse_playback_defaults(s).unwrap();
        assert_eq!(p(r#"{"MediaSources":[{"DefaultAudioStreamIndex":1,"DefaultSubtitleStreamIndex":-1}]}"#), (Some(1), None));
        assert_eq!(p(r#"{"MediaSources":[{"DefaultAudioStreamIndex":2}]}"#), (Some(2), None));
        assert_eq!(p(r#"{"MediaSources":[{"DefaultAudioStreamIndex":1,"DefaultSubtitleStreamIndex":3}]}"#), (Some(1), Some(3)));
        assert_eq!(p(r#"{"MediaSources":[]}"#), (None, None));
    }

    #[test]
    fn cultures_dedupe_by_code_and_sort_by_name() {
        let v: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"DisplayName":"French","ThreeLetterISOLanguageName":"fre"},{"DisplayName":"English","ThreeLetterISOLanguageName":"eng"},
                {"DisplayName":"French (Canada)","ThreeLetterISOLanguageName":"fre"},{"DisplayName":"None","ThreeLetterISOLanguageName":""}]"#,
        )
        .unwrap();
        assert_eq!(parse_cultures(&v), vec![("eng".to_string(), "English".to_string()), ("fre".to_string(), "French".to_string())]);
    }

    #[test]
    fn saving_keeps_the_rest_of_the_configuration() {
        let cfg = serde_json::json!({ "PlayDefaultAudioTrack": true, "AudioLanguagePreference": null });
        let out = with_lang_prefs(cfg, &LangPrefs { audio: "fre".into(), subtitle: "".into(), mode: "Smart".into() });
        assert_eq!(out["PlayDefaultAudioTrack"], true);
        assert_eq!((out["AudioLanguagePreference"].as_str(), out["SubtitleMode"].as_str()), (Some("fre"), Some("Smart")));
    }
}

/// Fields every list asks for: enough for a card, nothing more.
const CARD_FIELDS: &str = "PrimaryImageAspectRatio,SeriesInfo";

/// Download speed from `url` in bit/s: reads for up to `size_cap_secs` after the first
/// byte, so the server's own latency doesn't count. None when nothing arrives within 3 s
/// or less than 64 KiB arrived.
pub fn measure(url: &str, header: Option<&str>, size_cap_secs: f64) -> Option<u64> {
    use std::io::Read;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_global(Some(Duration::from_secs_f64(4.0 + size_cap_secs)))
        .build()
        .into();
    let t0 = Instant::now();
    let mut req = agent.get(url);
    if let Some(h) = header {
        req = req.header("Authorization", h);
    }
    let mut r = req.call().ok()?;
    let mut body = r.body_mut().as_reader();
    let mut buf = vec![0u8; 64 * 1024];
    if body.read(&mut buf).ok()? == 0 || t0.elapsed() > Duration::from_secs(3) {
        return None;
    }
    let t1 = Instant::now();
    let window = Duration::from_secs_f64(size_cap_secs);
    let mut n = 0u64;
    while t1.elapsed() < window {
        match body.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k as u64,
        }
    }
    let secs = t1.elapsed().as_secs_f64();
    (n >= 64 * 1024 && secs > 0.02).then(|| (n as f64 * 8.0 / secs) as u64)
}

pub struct Client {
    agent: ureq::Agent,
    pub auth: Auth,
}

impl Client {
    pub fn new(auth: Auth) -> Self {
        // a cold server can take tens of seconds for one query (it answers in
        // ~50 ms once warm): wait for it, but give up fast on an unreachable host
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        Client { agent, auth }
    }

    fn header(&self) -> String {
        format!(
            "MediaBrowser Client=\"PiFin\", Device=\"Raspberry Pi\", DeviceId=\"{}\", Version=\"{}\", Token=\"{}\"",
            self.auth.device_id,
            env!("CARGO_PKG_VERSION"),
            self.auth.token
        )
    }

    /// Throughput from the server in bit/s, measured on Jellyfin's bitrate test (see
    /// `measure`). Used when Play is pressed.
    pub fn link_rate(&self) -> Option<u64> {
        let url = format!("{}/Playback/BitrateTest?size=4000000", self.auth.server.trim_end_matches('/'));
        measure(&url, Some(&self.header()), 2.0)
    }

    /// The same measurement on a larger test file, for Settings > Network > Test connection.
    pub fn server_test(&self) -> Option<u64> {
        let url = format!("{}/Playback/BitrateTest?size=10000000", self.auth.server.trim_end_matches('/'));
        measure(&url, Some(&self.header()), 2.0)
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T, String> {
        let url = format!("{}{}", self.auth.server.trim_end_matches('/'), path);
        let mut req = self.agent.get(&url).header("Authorization", &self.header());
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        let mut resp = req.call().map_err(|e| format!("GET {path}: {e}"))?;
        resp.body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_json::<T>()
            .map_err(|e| format!("GET {path}: bad JSON: {e}"))
    }

    fn post(&self, path: &str, body: &serde_json::Value) -> Result<(), String> {
        let url = format!("{}{}", self.auth.server.trim_end_matches('/'), path);
        self.agent
            .post(&url)
            .header("Authorization", &self.header())
            .send_json(body)
            .map(|_| ())
            .map_err(|e| format!("POST {path}: {e}"))
    }

    /// Playback reporting (keeps Continue Watching and "now playing" right).
    pub fn report(&self, kind: &str, item_id: &str, pos_ticks: i64, paused: bool, session: Option<&str>) -> Result<(), String> {
        let path = match kind {
            "start" => "/Sessions/Playing",
            "stop" => "/Sessions/Playing/Stopped",
            _ => "/Sessions/Playing/Progress",
        };
        self.post(path, &serde_json::json!({
            "ItemId": item_id, "MediaSourceId": item_id, "PositionTicks": pos_ticks,
            "IsPaused": paused, "CanSeek": true,
            "PlayMethod": if session.is_some() { "Transcode" } else { "DirectPlay" },
            "PlaySessionId": session,
        }))
    }

    /// Stop the server-side conversion of a transcoded playback.
    pub fn stop_transcode(&self, session: &str) {
        let url = format!("{}/Videos/ActiveEncodings?DeviceId={}&PlaySessionId={session}",
            self.auth.server.trim_end_matches('/'), self.auth.device_id);
        let _ = self.agent.delete(&url).header("Authorization", &self.header()).call();
    }

    /// Store the resume position, or mark watched past 90 %.
    pub fn save_position(&self, item_id: &str, pos_ticks: i64, run_ticks: i64) -> Result<(), String> {
        let uid = &self.auth.user_id;
        if run_ticks > 0 && pos_ticks > run_ticks * 9 / 10 {
            self.post(&format!("/UserPlayedItems/{item_id}?userId={uid}"), &serde_json::json!({}))?;
            self.post(&format!("/UserItems/{item_id}/UserData?userId={uid}"), &serde_json::json!({"PlaybackPositionTicks": 0}))
        } else {
            self.post(&format!("/UserItems/{item_id}/UserData?userId={uid}"), &serde_json::json!({"PlaybackPositionTicks": pos_ticks}))
        }
    }

    pub fn item(&self, id: &str) -> Result<Item, String> {
        self.get(&format!("/Users/{}/Items/{id}", self.auth.user_id), &[])
    }

    /// The user's language preferences, and the whole Configuration they came from.
    pub fn lang_prefs(&self) -> Result<(LangPrefs, serde_json::Value), String> {
        let me: serde_json::Value = self.get("/Users/Me", &[])?;
        let cfg = me.get("Configuration").cloned().ok_or("no Configuration in /Users/Me")?;
        Ok((lang_prefs_of(&cfg), cfg))
    }

    /// Save the three language keys inside the full Configuration (the server replaces it whole).
    pub fn set_lang_prefs(&self, cfg: serde_json::Value, p: &LangPrefs) -> Result<(), String> {
        self.post(&format!("/Users/Configuration?userId={}", self.auth.user_id), &with_lang_prefs(cfg, p))
    }

    /// Languages the server knows: (3-letter code, display name), by name, no duplicates.
    pub fn cultures(&self) -> Result<Vec<(String, String)>, String> {
        let v: Vec<serde_json::Value> = self.get("/Localization/Cultures", &[])?;
        Ok(parse_cultures(&v))
    }

    /// The server's default (audio, subtitle) stream indexes for this user and item.
    pub fn playback_defaults(&self, item_id: &str) -> Result<(Option<i32>, Option<i32>), String> {
        let v: serde_json::Value = self.get(&format!("/Items/{item_id}/PlaybackInfo"), &[("userId", &self.auth.user_id)])?;
        parse_playback_defaults(&v.to_string())
    }

    /// A TV library's shows, most recently updated first.
    pub fn shows(&self, parent_id: &str) -> Result<Vec<Item>, String> {
        let p = format!("/Users/{}/Items", self.auth.user_id);
        let r: Items = self.get(&p, &[("ParentId", parent_id), ("Recursive", "true"), ("IncludeItemTypes", "Series"),
            ("SortBy", "DateLastContentAdded,SortName"), ("SortOrder", "Descending"), ("Limit", "40"), ("Fields", CARD_FIELDS)])?;
        Ok(r.items)
    }

    pub fn seasons(&self, series_id: &str) -> Result<Vec<Item>, String> {
        let r: Items = self.get(&format!("/Shows/{series_id}/Seasons"), &[("UserId", &self.auth.user_id)])?;
        Ok(r.items)
    }

    pub fn episodes(&self, series_id: &str, season_id: &str) -> Result<Vec<Item>, String> {
        let r: Items = self.get(&format!("/Shows/{series_id}/Episodes"), &[("UserId", &self.auth.user_id), ("SeasonId", season_id), ("Fields", "Overview,PrimaryImageAspectRatio")])?;
        Ok(r.items)
    }

    pub fn image_url(&self, item_id: &str, kind: &str, max_w: u32) -> String {
        format!(
            "{}/Items/{item_id}/Images/{kind}?maxWidth={max_w}&quality=85",
            self.auth.server.trim_end_matches('/')
        )
    }

    pub fn image(&self, item_id: &str, kind: &str, max_w: u32) -> Result<Vec<u8>, String> {
        let url = self.image_url(item_id, kind, max_w);
        let mut resp = self
            .agent
            .get(&url)
            .header("Authorization", &self.header())
            .call()
            .map_err(|e| format!("image {item_id}: {e}"))?;
        resp.body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| format!("image {item_id}: {e}"))
    }

    /// Name of the signed-in user and its avatar (None when it has none).
    pub fn me(&self, avatar_w: u32) -> Result<(String, Option<Vec<u8>>), String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Me {
            name: String,
            #[serde(default)]
            primary_image_tag: Option<String>,
        }
        let me: Me = self.get("/Users/Me", &[])?;
        let avatar = me.primary_image_tag.and_then(|_| {
            let url = format!("{}/Users/{}/Images/Primary?maxWidth={avatar_w}&quality=90", self.auth.server.trim_end_matches('/'), self.auth.user_id);
            let mut r = self.agent.get(&url).header("Authorization", &self.header()).call().ok()?;
            r.body_mut().with_config().limit(4 * 1024 * 1024).read_to_vec().ok()
        });
        Ok((me.name, avatar))
    }

    pub fn resume(&self) -> Result<Vec<Item>, String> {
        let p = format!("/Users/{}/Items/Resume", self.auth.user_id);
        let r: Items = self.get(&p, &[("Limit", "12"), ("Fields", CARD_FIELDS), ("MediaTypes", "Video"), ("EnableImageTypes", "Primary,Backdrop,Thumb")])?;
        Ok(r.items)
    }

    pub fn next_up(&self) -> Result<Vec<Item>, String> {
        let r: Items = self.get("/Shows/NextUp", &[("UserId", &self.auth.user_id), ("Limit", "12"), ("Fields", CARD_FIELDS)])?;
        Ok(r.items)
    }

    pub fn views(&self) -> Result<Vec<Item>, String> {
        let r: Items = self.get(&format!("/Users/{}/Views", self.auth.user_id), &[])?;
        Ok(r.items)
    }

    pub fn latest(&self, parent_id: &str) -> Result<Vec<Item>, String> {
        let p = format!("/Users/{}/Items/Latest", self.auth.user_id);
        self.get(&p, &[("ParentId", parent_id), ("Limit", "16"), ("Fields", CARD_FIELDS)])
    }

    pub fn search(&self, term: &str) -> Result<Vec<Item>, String> {
        let p = format!("/Users/{}/Items", self.auth.user_id);
        let r: Items = self.get(&p, &[("SearchTerm", term), ("Recursive", "true"), ("IncludeItemTypes", "Movie,Series,Episode"), ("Limit", "24"), ("Fields", CARD_FIELDS)])?;
        Ok(r.items)
    }

    /// One page of a library, sorted by name.
    pub fn library(&self, parent_id: &str, start: usize, limit: usize) -> Result<(Vec<Item>, i64), String> {
        let p = format!("/Users/{}/Items", self.auth.user_id);
        let (s, l) = (start.to_string(), limit.to_string());
        let r: Items = self.get(&p, &[("ParentId", parent_id), ("Recursive", "true"), ("IncludeItemTypes", "Movie,Series"), ("SortBy", "SortName"), ("StartIndex", &s), ("Limit", &l), ("Fields", CARD_FIELDS)])?;
        Ok((r.items, r.total_record_count))
    }
}


/// Jellyfin Quick Connect: the TV shows a code, the user approves it from any
/// signed-in Jellyfin app (Settings → Quick Connect). No password typing on a TV.
pub struct QuickConnect {
    agent: ureq::Agent,
    server: String,
    device_id: String,
    pub code: String,
    secret: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QcState {
    secret: String,
    code: String,
    #[serde(default)]
    authenticated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthResult {
    access_token: String,
    user: AuthUser,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthUser {
    id: String,
    #[serde(default)]
    name: String,
}

impl QuickConnect {
    fn header(device_id: &str) -> String {
        format!(
            "MediaBrowser Client=\"PiFin\", Device=\"Raspberry Pi\", DeviceId=\"{device_id}\", Version=\"{}\"",
            env!("CARGO_PKG_VERSION")
        )
    }

    pub fn start(server: &str, device_id: &str) -> Result<Self, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build().into();
        let server = server.trim_end_matches('/').to_string();
        let mut r = agent
            .post(&format!("{server}/QuickConnect/Initiate"))
            .header("Authorization", &Self::header(device_id))
            .send_empty()
            .map_err(|e| format!("Quick Connect: {e} (is it enabled on the server?)"))?;
        let st: QcState = r.body_mut().read_json().map_err(|e| format!("Quick Connect: {e}"))?;
        Ok(QuickConnect { agent, server, device_id: device_id.into(), code: st.code, secret: st.secret })
    }

    /// Ok(Some((auth, user name))) once approved, Ok(None) while waiting.
    pub fn poll(&self) -> Result<Option<(Auth, String)>, String> {
        let mut r = self
            .agent
            .get(&format!("{}/QuickConnect/Connect", self.server))
            .query("Secret", &self.secret)
            .call()
            .map_err(|e| format!("Quick Connect: {e}"))?;
        let st: QcState = r.body_mut().read_json().map_err(|e| format!("Quick Connect: {e}"))?;
        if !st.authenticated {
            return Ok(None);
        }
        let mut r = self
            .agent
            .post(&format!("{}/Users/AuthenticateWithQuickConnect", self.server))
            .header("Authorization", &Self::header(&self.device_id))
            .send_json(serde_json::json!({ "Secret": self.secret }))
            .map_err(|e| format!("Quick Connect: {e}"))?;
        let a: AuthResult = r.body_mut().read_json().map_err(|e| format!("Quick Connect: {e}"))?;
        let auth = Auth { server: self.server.clone(), user_id: a.user.id, token: a.access_token, device_id: self.device_id.clone() };
        Ok(Some((auth, a.user.name)))
    }
}
