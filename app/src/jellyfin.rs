//! Minimal blocking Jellyfin client: only the calls the UI needs, small responses.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Auth {
    pub server: String,
    pub user_id: String,
    pub token: String,
    pub device_id: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    #[serde(rename = "Type")]
    pub kind: String,
    pub primary_image_tag: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct UserData {
    pub playback_position_ticks: i64,
    pub played: bool,
    pub played_percentage: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Item {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
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
    pub genres: Vec<String>,
    pub community_rating: Option<f64>,
    pub official_rating: Option<String>,
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

/// Fields every list asks for: enough for a card, nothing more.
const CARD_FIELDS: &str = "PrimaryImageAspectRatio,SeriesInfo";

pub struct Client {
    agent: ureq::Agent,
    pub auth: Auth,
}

impl Client {
    pub fn new(auth: Auth) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
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
    pub fn report(&self, kind: &str, item_id: &str, pos_ticks: i64, paused: bool) -> Result<(), String> {
        let path = match kind {
            "start" => "/Sessions/Playing",
            "stop" => "/Sessions/Playing/Stopped",
            _ => "/Sessions/Playing/Progress",
        };
        self.post(path, &serde_json::json!({
            "ItemId": item_id, "MediaSourceId": item_id, "PositionTicks": pos_ticks,
            "IsPaused": paused, "CanSeek": true, "PlayMethod": "DirectPlay",
        }))
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

    /// Ok(Some(auth)) once approved, Ok(None) while waiting.
    pub fn poll(&self) -> Result<Option<Auth>, String> {
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
        Ok(Some(Auth { server: self.server.clone(), user_id: a.user.id, token: a.access_token, device_id: self.device_id.clone() }))
    }
}
