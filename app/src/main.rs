//! PiFin: lightweight Jellyfin browser for the Raspberry Pi.
//!
//! Runs under cage as a Wayland client. Playback is handed off to the native
//! mpv process (hardware decoding + pifin-osd.lua overlay); the UI does no work
//! while mpv plays and comes back on the same screen when it exits.
//!
//! Threads: UI (Slint, owns all models), one API thread, two image workers.
//! Workers hand results back with `invoke_from_event_loop`; a generation counter
//! drops backdrop results that arrive after the focus moved on.

mod jellyfin;
mod modeswitch;
mod profiles;
#[allow(dead_code)] // the Settings UI (later tasks) uses the rest of the API
mod settings;
mod settings_ui;

use jellyfin::{Auth, Client, Item};
use slint::{Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::cell::RefCell;
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

slint::include_modules!();

/// Where pifin-osd.lua and mpv-input.conf live on the Pi.
const MPV_DATA: &str = "/opt/pifin-remote";
/// Poster size in physical px (150x225 logical at scale 1.5).
const CARD_W: u32 = 225;
const CARD_H: u32 = 338;
const BACKDROP_W: u32 = 1280;

/// Per-playback mpv options from the settings (and, later, the chosen tracks).
struct MpvOpts {
    sub_scale: f32,
    passthrough: bool,
    aid: Option<String>,
    sid: Option<String>,
    /// HDR source played with the 4K & HDR setting on: ask the compositor for HDR output.
    hdr_hint: bool,
}

impl MpvOpts {
    fn from_settings() -> MpvOpts {
        let d = settings::current().display.clone();
        MpvOpts { sub_scale: d.subtitle_scale, passthrough: d.audio_passthrough, aid: None, sid: None, hdr_hint: false }
    }
}


/// mpv arguments proven on the Pi 4 (0 dropped frames on HEVC 10-bit 1080p).
fn mpv_command(url: &str, start_s: f64, title: &str, opts: &MpvOpts) -> Command {
    let mut c = Command::new("mpv");
    c.args([
        "--fs", "--vo=gpu", "--gpu-context=wayland", "--profile=fast",
        "--hwdec=drm,v4l2m2m-copy,no", "--cache=yes", "--demuxer-max-bytes=150MiB",
        // weak Wi-Fi: read ahead, and after an underrun wait for a few seconds of
        // buffer instead of stuttering every couple of seconds
        "--demuxer-readahead-secs=20", "--cache-pause-wait=4",
        "--osc=no", "--osd-level=0", "--really-quiet",
    ]);
    c.arg(format!("--script={MPV_DATA}/pifin-osd.lua"));
    c.arg(format!("--input-conf={MPV_DATA}/mpv-input.conf"));
    c.arg("--input-ipc-server=/tmp/pifin-mpv.sock");
    c.arg(format!("--force-media-title={title}"));
    if start_s > 0.0 {
        c.arg(format!("--start={start_s:.1}"));
    }
    if opts.sub_scale != 1.0 {
        c.arg(format!("--sub-scale={:.2}", opts.sub_scale));
    }
    if opts.passthrough {
        c.arg("--audio-spdif=ac3,eac3,dts,dts-hd,truehd");
    }
    if let Some(a) = &opts.aid {
        c.arg(format!("--aid={a}"));
    }
    if let Some(s) = &opts.sid {
        c.arg(format!("--sid={s}"));
    }
    if opts.hdr_hint {
        // mpv >= 0.38 (Pi: 0.40); a no-op where the compositor has no colour management
        c.arg("--target-colorspace-hint=yes");
    }
    c.arg(url);
    c
}


/// Background colour of the UI (#0b0b12), used to bake the backdrop fade.
const BG: [f32; 3] = [11.0, 11.0, 18.0];

/// Decode an image off the UI thread and prepare it so the GPU only has to draw
/// a plain opaque/textured quad: no clipping, no opacity, no gradient layers.
fn decode(bytes: &[u8], slot: Slot, w: u32, h: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut rgba = img.resize_to_fill(w, h, image::imageops::FilterType::Triangle).to_rgba8();
    let (iw, ih) = rgba.dimensions();
    match slot {
        // rounded corners baked into the alpha channel
        Slot::Card { .. } => {
            let r = 14.0f32;
            for (x, y, px) in rgba.enumerate_pixels_mut() {
                let cx = if (x as f32) < r { r - x as f32 - 0.5 } else if x as f32 >= iw as f32 - r { x as f32 + 0.5 - (iw as f32 - r) } else { 0.0 };
                let cy = if (y as f32) < r { r - y as f32 - 0.5 } else if y as f32 >= ih as f32 - r { y as f32 + 0.5 - (ih as f32 - r) } else { 0.0 };
                if cx > 0.0 && cy > 0.0 {
                    let d = (cx * cx + cy * cy).sqrt() - r;
                    let a = (0.5 - d).clamp(0.0, 1.0);
                    px.0[3] = (px.0[3] as f32 * a) as u8;
                }
            }
        }
        // dimmed, faded into the background on the left and bottom
        Slot::Backdrop { .. } => {
            for (x, y, px) in rgba.enumerate_pixels_mut() {
                let fx = x as f32 / iw as f32;
                let fy = y as f32 / ih as f32;
                let horiz = (0.10 + 0.32 * ((fx - 0.25) / 0.6).clamp(0.0, 1.0)).min(0.42);
                let vert = 1.0 - ((fy - 0.55) / 0.45).clamp(0.0, 1.0) * 0.8;
                let a = horiz * vert;
                for c in 0..3 {
                    px.0[c] = (px.0[c] as f32 * a + BG[c] * (1.0 - a)) as u8;
                }
                px.0[3] = 255;
            }
        }
    }
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), iw, ih))
}

/// What an image result is for. Cards are addressed by screen id so results
/// still land correctly after the user moved to another screen and back.
#[derive(Clone, Copy)]
enum Slot {
    Card { screen: u64, row: usize, col: usize },
    Backdrop { gen: u64 },
}

/// ~/.cache/pifin (or $XDG_CACHE_HOME/pifin).
fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    std::path::PathBuf::from(base).join("pifin")
}

/// The last full home screen of a user, shown at once at the next start while
/// the server is asked again (a cold server took 87 s once; the network itself
/// comes up ~7 s after the app at boot).
fn home_cache(user_id: &str) -> std::path::PathBuf {
    cache_dir().join(format!("home-{user_id}.json"))
}

fn save_home(user_id: &str, rows: &Rows) {
    let p = home_cache(user_id);
    let _ = std::fs::create_dir_all(cache_dir());
    let tmp = p.with_extension("tmp");
    if serde_json::to_vec(rows).ok().is_some_and(|b| std::fs::write(&tmp, b).is_ok()) {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn load_home_cache(user_id: &str) -> Option<Rows> {
    serde_json::from_slice(&std::fs::read(home_cache(user_id)).ok()?).ok().filter(|r: &Rows| !r.is_empty())
}

/// Latest backdrop request: workers skip older ones still in the queue.
static BACKDROP_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Server-resized images kept on disk (~25 KB a poster, ~300 KB a backdrop):
/// the home screen fills from the SD card at boot, and a slow or remote server
/// (a hotel, a friend's place) is asked again only after a week.
mod img_cache {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};
    const FRESH: Duration = Duration::from_secs(7 * 86400);
    const MAX_BYTES: u64 = 256 * 1024 * 1024;

    fn dir() -> PathBuf {
        super::cache_dir().join("img")
    }

    fn age(p: &PathBuf) -> Option<Duration> {
        std::fs::metadata(p).ok()?.modified().ok()?.elapsed().ok()
    }

    /// Cached bytes when fresh, else `fetch` (saved), else stale bytes if any.
    pub fn get(key: &str, fetch: impl FnOnce() -> Result<Vec<u8>, String>) -> Option<Vec<u8>> {
        let p = dir().join(key);
        if age(&p).is_some_and(|a| a < FRESH) {
            if let Ok(b) = std::fs::read(&p) {
                return Some(b);
            }
        }
        match fetch() {
            Ok(b) => {
                let _ = std::fs::create_dir_all(dir());
                let tmp = p.with_extension("tmp");
                if std::fs::write(&tmp, &b).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
                Some(b)
            }
            Err(e) => {
                eprintln!("{e}");
                std::fs::read(&p).ok()
            }
        }
    }

    /// Keep the cache under MAX_BYTES, oldest files out first (run once at start).
    pub fn prune() {
        let Ok(rd) = std::fs::read_dir(dir()) else { return };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, m.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort();
        for (_, len, p) in files {
            if total <= MAX_BYTES {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total -= len;
            }
        }
    }
}

struct ImgJob {
    slot: Slot,
    item_id: String,
    kind: &'static str,
    w: u32,
    h: u32,
}

/// One browsable screen: rows of items (home, or a series' seasons).
struct Screen {
    id: u64,
    titles: Vec<String>,
    items: Vec<Vec<Item>>,
    cards: Vec<Rc<VecModel<Card>>>,
    focus: Vec<i32>,
    cur_row: i32,
    search: Option<String>, // Some(query) for the search screen
}

/// UI-thread state.
struct State {
    screens: Vec<Screen>,
    rows: Rc<VecModel<Row>>,
    next_id: u64,
    backdrop_gen: u64,
    img_tx: mpsc::Sender<ImgJob>,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

fn with_state(f: impl FnOnce(&Rc<RefCell<State>>)) {
    STATE.with(|s| {
        if let Some(st) = s.borrow().as_ref() {
            f(st)
        }
    })
}

fn card_of(it: &Item, in_series: bool) -> Card {
    let progress = match (&it.user_data, it.run_time_ticks) {
        (Some(u), Some(run)) if run > 0 && u.playback_position_ticks > 0 => {
            (u.playback_position_ticks as f64 / run as f64).clamp(0.0, 1.0) as f32
        }
        _ => 0.0,
    };
    let title = if let Some(a) = it.action {
        match (a, &it.user_data) {
            ("resume", Some(u)) => format!("▶  Resume from {}", fmt_ticks(u.playback_position_ticks)),
            ("resume", None) => "▶  Play".into(),
            _ => "↺  Play from start".into(),
        }
    } else if in_series || it.kind == "Person" || (it.kind == "Episode" && it.series_name.is_none()) {
        it.name.clone()
    } else {
        it.title().to_string()
    };
    Card {
        id: it.id.clone().into(),
        title: title.into(),
        subtitle: if it.kind == "Person" {
            it.overview.clone().unwrap_or_default().into()
        } else if it.action.is_some() {
            "".into()
        } else if in_series {
            let mut v = vec![];
            if let Some(e) = it.index_number { v.push(format!("Episode {e}")); }
            if let Some(t) = it.run_time_ticks.filter(|t| *t >= 600_000_000) { v.push(format!("{} min", t / 600_000_000)); }
            v.join(" · ").into()
        } else {
            it.subtitle().into()
        },
        image: Default::default(),
        has_image: false,
        progress,
    }
}

/// Poster for a card: the item's own, else the series poster (episodes on home).
fn poster_id(it: &Item, own: bool) -> String {
    if (own || it.kind != "Episode") && it.image_tags.contains_key("Primary") {
        it.id.clone()
    } else {
        it.series_id.clone().unwrap_or_else(|| it.id.clone())
    }
}

fn backdrop_id(it: &Item) -> String {
    if !it.backdrop_image_tags.is_empty() {
        it.id.clone()
    } else {
        it.series_id.clone().unwrap_or_else(|| it.id.clone())
    }
}

fn fmt_ticks(t: i64) -> String {
    let s = t / 10_000_000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn info_line(it: &Item) -> String {
    let mut parts = vec![];
    if it.kind == "Episode" {
        parts.push(it.subtitle());
    } else if let Some(y) = it.production_year {
        parts.push(y.to_string());
    }
    if let Some(t) = it.run_time_ticks.filter(|t| *t >= 600_000_000) {
        let m = t / 600_000_000;
        parts.push(if m >= 60 { format!("{}h {:02}m", m / 60, m % 60) } else { format!("{m} min") });
    }
    if let Some(u) = &it.user_data {
        if u.playback_position_ticks > 0 {
            parts.push(format!("resume at {}", fmt_ticks(u.playback_position_ticks)));
        } else if u.played {
            parts.push("watched".into());
        }
    }
    parts.join("  ·  ")
}

/// Rows for the home screen.
type Rows = Vec<(String, Vec<Item>)>;

/// Home rows, all requests in parallel. A cold server can take over a minute for
/// one query (Continue Watching took 81 s once) while the others answer in
/// 50 ms, so `publish(rows)` gets what is there after `PARTIAL_AFTER`, then the
/// full set when the last request finishes. Returns false if any request failed.
fn fetch_home(client: &Arc<Client>, partial: bool, mut publish: impl FnMut(Rows)) -> bool {
    const PARTIAL_AFTER: Duration = Duration::from_millis(1500);
    type Res = Result<Vec<Item>, String>;
    enum Msg {
        Views(Result<Vec<Item>, String>),
        Row(usize, Res),
    }
    let (tx, rx) = mpsc::channel::<Msg>();
    let spawn = |slot: usize, f: Box<dyn FnOnce(&Client) -> Res + Send>| {
        let (tx, client) = (tx.clone(), client.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Row(slot, f(&client)));
        });
    };
    let mut slots: Vec<(String, Option<Res>)> = vec![("Continue Watching".into(), None), ("Next Up".into(), None)];
    spawn(0, Box::new(|c| c.resume()));
    spawn(1, Box::new(|c| c.next_up()));
    {
        let (tx, client) = (tx.clone(), client.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Views(client.views()));
        });
    }
    let t0 = Instant::now();
    let mut pending = 3;
    let mut ok = true;
    let mut published = false;
    let rows_of = |slots: &[(String, Option<Res>)]| -> Rows {
        slots.iter().filter_map(|(t, r)| match r {
            Some(Ok(v)) if !v.is_empty() => Some((t.clone(), v.clone())),
            _ => None,
        }).collect()
    };
    while pending > 0 {
        let left = PARTIAL_AFTER.saturating_sub(t0.elapsed());
        let wait = if !published && !left.is_zero() { left } else { Duration::from_secs(120) };
        match rx.recv_timeout(wait) {
            Ok(Msg::Views(Ok(views))) => {
                pending -= 1;
                for v in views {
                    let title = match v.collection_type.as_deref() {
                        Some("movies") => format!("Latest {}", v.name),
                        Some("tvshows") => v.name.clone(),
                        _ => continue,
                    };
                    let slot = slots.len();
                    slots.push((title, None));
                    pending += 1;
                    let (id, movies) = (v.id.clone(), v.collection_type.as_deref() == Some("movies"));
                    spawn(slot, Box::new(move |c| if movies { c.latest(&id) } else { c.shows(&id) }));
                }
            }
            Ok(Msg::Views(Err(e))) => {
                eprintln!("{e}");
                ok = false;
                pending -= 1;
            }
            Ok(Msg::Row(slot, r)) => {
                if let Err(e) = &r {
                    eprintln!("{e}");
                    ok = false;
                }
                slots[slot].1 = Some(r);
                pending -= 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if partial && !published && pending > 0 && t0.elapsed() >= PARTIAL_AFTER {
            let rows = rows_of(&slots);
            if !rows.is_empty() {
                eprintln!("HOME_PARTIAL_MS={} rows={} pending={pending}", t0.elapsed().as_millis(), rows.len());
                publish(rows);
                published = true;
            }
        }
    }
    let rows = rows_of(&slots);
    if !rows.is_empty() {
        publish(rows);
    }
    ok
}

/// A show's page: one row of seasons.
fn fetch_seasons(client: &Client, series_id: &str) -> Vec<(String, Vec<Item>)> {
    match client.seasons(series_id) {
        Ok(v) if !v.is_empty() => vec![("Seasons".to_string(), v)],
        Ok(_) => vec![],
        Err(e) => {
            eprintln!("{e}");
            vec![]
        }
    }
}

/// A season's page: its episodes in order.
fn fetch_season(client: &Client, season: &Item) -> Vec<(String, Vec<Item>)> {
    let series_id = season.series_id.clone().unwrap_or_default();
    match client.episodes(&series_id, &season.id) {
        Ok(v) if !v.is_empty() => vec![(format!("{} · {}", season.series_name.clone().unwrap_or_default(), season.name), v)],
        Ok(_) => vec![],
        Err(e) => {
            eprintln!("{e}");
            vec![]
        }
    }
}

/// Rows for a movie page: Play/Resume actions, then the cast.
fn fetch_movie(client: &Client, id: &str) -> Vec<(String, Vec<Item>)> {
    let Ok(movie) = client.item(id) else { return vec![] };
    let resume = movie.user_data.as_ref().map(|u| u.playback_position_ticks > 0).unwrap_or(false);
    let mut actions = vec![];
    let mut a = movie.clone();
    a.action = Some("resume");
    if !resume {
        a.user_data = None;
    }
    actions.push(a);
    if resume {
        let mut b = movie.clone();
        b.action = Some("start");
        b.user_data = None;
        actions.push(b);
    }
    let mut rows = vec![("Watch".to_string(), actions)];
    let cast: Vec<Item> = movie.people.iter().filter(|p| p.kind == "Actor" || p.kind == "Director").take(20).map(|p| Item {
        id: p.id.clone(),
        name: p.name.clone(),
        kind: "Person".into(),
        overview: p.role.clone().filter(|r| !r.is_empty()).or_else(|| (p.kind == "Director").then(|| "Director".into())),
        image_tags: p.primary_image_tag.iter().map(|t| ("Primary".to_string(), t.clone())).collect(),
        ..Default::default()
    }).collect();
    if !cast.is_empty() {
        rows.push(("Cast & crew".to_string(), cast));
    }
    rows
}

/// Push (or, with `replace`, swap in place) a screen built from rows, queue its images.
fn install_screen(app: &App, st: &mut State, rows: Vec<(String, Vec<Item>)>, own_images: bool, replace: Option<usize>) {
    let id = st.next_id;
    st.next_id += 1;
    let mut screen = Screen { id, titles: vec![], items: vec![], cards: vec![], focus: vec![], cur_row: 0, search: None };
    for (title, items) in rows {
        screen.cards.push(Rc::new(VecModel::from(items.iter().map(|i| card_of(i, own_images)).collect::<Vec<_>>())));
        screen.titles.push(title);
        screen.focus.push(0);
        screen.items.push(items);
    }
    // images: first 8 of every row first (what is on screen), then the rest
    for pass in 0..2 {
        for (ri, items) in screen.items.iter().enumerate() {
            for (ci, it) in items.iter().enumerate() {
                if (pass == 0) == (ci < 8) {
                    let _ = st.img_tx.send(ImgJob {
                        slot: Slot::Card { screen: id, row: ri, col: ci },
                        item_id: poster_id(it, own_images),
                        kind: "Primary",
                        w: CARD_W,
                        h: CARD_H,
                    });
                }
            }
        }
    }
    match replace {
        Some(i) if i < st.screens.len() => {
            // keep the user's place when refreshing
            // (rows matched by title: a late row such as Continue Watching can
            // appear above the others)
            let old = &st.screens[i];
            let old_title = old.titles.get(old.cur_row as usize);
            screen.cur_row = old_title
                .and_then(|t| screen.titles.iter().position(|n| n == t))
                .map(|r| r as i32)
                .unwrap_or_else(|| old.cur_row.min(screen.items.len() as i32 - 1).max(0));
            for (r, f) in screen.focus.iter_mut().enumerate() {
                if let Some(of) = old.titles.iter().position(|t| *t == screen.titles[r]).and_then(|o| old.focus.get(o)) {
                    *f = (*of).min(screen.items[r].len() as i32 - 1).max(0);
                }
            }
            screen.search = old.search.clone();
            st.screens[i] = screen;
            if i == st.screens.len() - 1 {
                show_top(app, st);
            }
        }
        _ => {
            st.screens.push(screen);
            show_top(app, st);
        }
    }
}

/// Point the UI's rows at the top screen and restore its focus.
fn show_top(app: &App, st: &mut State) {
    let Some(sc) = st.screens.last() else { return };
    let rows: Vec<Row> = sc
        .titles
        .iter()
        .zip(&sc.cards)
        .zip(&sc.focus)
        .map(|((t, c), f)| Row { title: t.clone().into(), cards: ModelRc::from(c.clone()), focus: *f })
        .collect();
    st.rows.set_vec(rows);
    app.set_search_mode(sc.search.is_some());
    app.set_nav_show(st.screens.len() == 1 && sc.search.is_none());
    app.set_nav_focus(-1);
    if let Some(q) = &sc.search {
        app.set_query(q.clone().into());
        if sc.items.is_empty() {
            app.set_kb_active(true);
        }
    }
    let (r, c) = (sc.cur_row, sc.focus.get(sc.cur_row as usize).copied().unwrap_or(0));
    app.set_cur_row(r);
    let app_weak = app.as_weak();
    // run the focus update after this borrow ends
    slint::Timer::single_shot(Duration::from_millis(0), move || {
        if let Some(a) = app_weak.upgrade() {
            a.invoke_moved(r, c);
        }
    });
}

/// Talk to the running mpv over its JSON IPC socket (blocking; used off the UI thread).
fn mpv_get(prop: &str) -> Option<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::os::unix::net::UnixStream::connect("/tmp/pifin-mpv.sock").ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(format!("{{\"command\":[\"get_property\",\"{prop}\"]}}\n").as_bytes()).ok()?;
    let mut lines = BufReader::new(s).lines();
    while let Some(Ok(line)) = lines.next() {
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("event").is_none() {
            return v.get("data").cloned();
        }
    }
    None
}

/// Files above this bitrate are converted by the server instead of played directly:
/// the Pi's Wi-Fi can't keep up with Blu-ray remuxes (30+ Mbit/s).
const MAX_DIRECT_BITRATE: i64 = 16_000_000;
/// Converted streams: at most this bitrate (video + audio).
const MAX_TRANSCODE_BITRATE: i64 = 10_000_000;

/// Last measured throughput from the server (bit/s, 0 = unknown) and when. Measured
/// when Play is pressed (a minute's result is reused), so a weak hotel or home
/// Wi-Fi gets a stream it can carry instead of freezing every few seconds.
static LINK: Mutex<Option<(Instant, u64)>> = Mutex::new(None);

fn link_bps() -> Option<u64> {
    LINK.lock().ok()?.filter(|(t, _)| t.elapsed() < Duration::from_secs(60)).map(|(_, b)| b)
}

/// (largest file bitrate played directly, bitrate of a converted stream) for a link.
/// Direct play may use 80% of the link (it reads ahead in bursts); a converted stream
/// gets 60%, since Wi-Fi swings by half from one minute to the next.
fn bitrate_budget(link: u64) -> (i64, i64) {
    if link == 0 {
        return (MAX_DIRECT_BITRATE, MAX_TRANSCODE_BITRATE);
    }
    let l = link as f64;
    (((l * 0.8) as i64).min(MAX_DIRECT_BITRATE), ((l * 0.6) as i64).clamp(1_500_000, MAX_TRANSCODE_BITRATE))
}

/// How to play an item: the URL, extra subtitle files, and the track names the
/// phone remote shows (in mpv's order).
struct Plan {
    url: String,
    subs: Vec<String>,
    session: Option<String>,
    audio_names: Vec<String>,
    sub_names: Vec<String>,
    /// shown while starting when the connection forced a lower quality
    note: String,
    /// mpv's track numbers for the server's default tracks (None = leave mpv's choice)
    aid: Option<String>,
    sid: Option<String>,
    /// index into `subs` of the subtitle to switch on once it is added
    select_sub: Option<usize>,
}

#[derive(Debug, Default, PartialEq)]
struct Tracks {
    aid: Option<String>,
    sid: Option<String>,
    select_sub: Option<usize>,
}

/// The tracks the server's defaults ask for, in mpv's terms. Direct play: mpv numbers the
/// file's own tracks from 1 in file order (the order Jellyfin lists them); an external
/// text subtitle is added later and selected there. Converted: the stream carries one audio
/// track and every text subtitle is added as a file. A default that is not found leaves
/// mpv's own choice; no default subtitle means off.
fn tracks(streams: &[&jellyfin::MediaStream], defaults: Option<(Option<i32>, Option<i32>)>, convert: bool) -> Tracks {
    let Some((audio, sub)) = defaults else { return Tracks::default() };
    let mut t = Tracks::default();
    if !convert {
        let internal_audio = streams.iter().filter(|s| s.kind == "Audio" && !s.is_external);
        t.aid = audio.and_then(|i| internal_audio.map(|s| s.index).position(|x| x == i)).map(|p| (p + 1).to_string());
    }
    match sub {
        None => t.sid = Some("no".into()),
        Some(i) => {
            let internal = streams.iter().filter(|s| s.kind == "Subtitle" && !s.is_external);
            if let Some(p) = internal.map(|s| s.index).position(|x| x == i).filter(|_| !convert) {
                t.sid = Some((p + 1).to_string());
                return t;
            }
            let files = streams.iter().filter(|s| s.kind == "Subtitle" && s.is_text_subtitle_stream && (convert || s.is_external));
            t.select_sub = files.map(|s| s.index).position(|x| x == i);
        }
    }
    t
}

/// What to do with a file: play it as is, or have the server convert it.
#[derive(Debug, PartialEq)]
enum Decision {
    Direct,
    Convert { target: i64, max_height: i32 },
}

/// The link's budget, lowered by the user's limits (global or per network).
fn decide(source_bitrate: i64, source_height: Option<i32>, link: u64, lim: &settings::Playback) -> Decision {
    let (mut direct_max, mut target) = bitrate_budget(link);
    if lim.max_bitrate > 0 {
        direct_max = direct_max.min(lim.max_bitrate);
        target = target.min(lim.max_bitrate);
    }
    let target = target.clamp(1_500_000, MAX_TRANSCODE_BITRATE);
    if lim.prefer_original && source_bitrate <= direct_max && source_height.is_none_or(|h| h <= lim.max_height) {
        Decision::Direct
    } else {
        Decision::Convert { target, max_height: lim.max_height }
    }
}

/// Widest picture for a height cap.
fn width_for_height(h: i32) -> i32 {
    match h {
        2160.. => 3840,
        1080..=2159 => 1920,
        720..=1079 => 1280,
        _ => 854,
    }
}


/// True once per distinct new `jellyfin_url` (seen after startup) that differs from the
/// running server and while no player runs. `seen` is updated only when it fires, so a
/// URL the daemon could not apply to auth.json cannot restart the app in a loop.
fn server_changed(seen: &mut String, url: &str, server: &str, idle: bool) -> bool {
    let norm = |s: &str| s.trim_end_matches('/').to_string();
    if url.is_empty() || !idle || norm(url) == norm(seen) || norm(url) == norm(server) {
        return false;
    }
    *seen = url.to_string();
    true
}


fn plan(auth: &Auth, it: &Item, link: u64, lim: &settings::Playback) -> Plan {
    let server = auth.server.trim_end_matches('/');
    let token = &auth.token;
    let ms = it.media_sources.first();
    let msid = ms.map(|m| m.id.clone()).unwrap_or_else(|| it.id.clone());
    let streams: Vec<&jellyfin::MediaStream> = ms.map(|m| m.media_streams.iter().collect()).unwrap_or_default();
    let name = |s: &jellyfin::MediaStream| s.display_title.clone().or(s.title.clone()).unwrap_or_else(|| format!("Track {}", s.index));
    let sub_url = |s: &jellyfin::MediaStream| format!("{server}/Videos/{}/{msid}/Subtitles/{}/0/Stream.srt?ApiKey={token}", it.id, s.index);
    let bitrate = ms.and_then(|m| m.bitrate).unwrap_or(0);
    let height = streams.iter().find(|s| s.kind == "Video").and_then(|s| s.height);
    let (link_direct_max, link_target) = bitrate_budget(link);
    let decision = decide(bitrate, height, link, lim);
    let tr = tracks(&streams, it.defaults, matches!(decision, Decision::Convert { .. }));
    eprintln!("PLAN id={} source_bitrate={bitrate} height={height:?} link={link} direct_max={link_direct_max} target={link_target} limits={lim:?} -> {decision:?} defaults={:?} aid={:?} sid={:?} select_sub={:?}", it.id, it.defaults, tr.aid, tr.sid, tr.select_sub);
    let Decision::Convert { target, max_height } = decision else {
        // direct play: internal tracks in file order, then external text subs
        let audio_names = streams.iter().filter(|s| s.kind == "Audio" && !s.is_external).map(|s| name(s)).collect();
        let mut sub_names: Vec<String> = streams.iter().filter(|s| s.kind == "Subtitle" && !s.is_external).map(|s| name(s)).collect();
        let ext: Vec<&&jellyfin::MediaStream> = streams.iter().filter(|s| s.kind == "Subtitle" && s.is_external && s.is_text_subtitle_stream).collect();
        sub_names.extend(ext.iter().map(|s| name(s)));
        return Plan {
            url: format!("{server}/Videos/{}/stream?static=true&mediaSourceId={msid}&api_key={token}", it.id),
            subs: ext.iter().map(|s| sub_url(s)).collect(),
            session: None,
            audio_names,
            sub_names,
            note: String::new(),
            aid: tr.aid,
            sid: tr.sid,
            select_sub: tr.select_sub,
            };
    };
    // converted by the server (hardware encoder on the server side), HLS so seeking works
    let session = std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default().trim().replace('-', "");
    let wanted = it.defaults.and_then(|d| d.0).and_then(|i| streams.iter().find(|s| s.kind == "Audio" && s.index == i));
    let audio = wanted.or(streams.iter().find(|s| s.kind == "Audio" && s.is_default)).or(streams.iter().find(|s| s.kind == "Audio"));
    let text_subs: Vec<&&jellyfin::MediaStream> = streams.iter().filter(|s| s.kind == "Subtitle" && s.is_text_subtitle_stream).collect();
    // a weak link gets a lighter stream: less audio, and 720p below ~3.5 Mbit/s
    // (sharper than a starved 1080p)
    let audio_br: i64 = if target < 6_000_000 { 256_000 } else { 384_000 };
    let video_br = (target - audio_br).max(1_200_000);
    let width = (if video_br < 3_500_000 { 1280 } else { 1920 }).min(width_for_height(max_height));
    eprintln!("TRANSCODE id={} video={video_br} audio={audio_br} width={width} max_height={max_height} subs={} burn_in={:?}", it.id, text_subs.len(), burn_in(&streams, it.defaults));
    // only the link is named: no note when just the user's limits caused the conversion
    let note = if bitrate > link_direct_max && (link_target < MAX_TRANSCODE_BITRATE || bitrate <= MAX_DIRECT_BITRATE) {
        format!("lower quality for this connection (≈{} Mbit/s)", (link + 500_000) / 1_000_000)
    } else {
        String::new()
    };
    Plan {
        url: format!(
            "{server}/Videos/{}/master.m3u8?MediaSourceId={msid}&DeviceId={}&PlaySessionId={session}&ApiKey={token}\
             &VideoCodec=h264&AudioCodec=aac&VideoBitrate={video_br}&AudioBitrate={audio_br}&MaxAudioChannels=6\
             &TranscodingMaxAudioChannels=6&MaxWidth={width}&MaxHeight={max_height}&SegmentContainer=ts&MinSegments=1&BreakOnNonKeyFrames=True{}{}",
            it.id, auth.device_id,
            audio.map(|a| format!("&AudioStreamIndex={}", a.index)).unwrap_or_default(),
            burn_in(&streams, it.defaults)
        ),
        note,
        subs: text_subs.iter().map(|s| sub_url(s)).collect(),
        session: Some(session),
        audio_names: audio.map(|a| vec![format!("{} (converted)", name(a))]).unwrap_or_default(),
        sub_names: text_subs.iter().map(|s| name(s)).collect(),
        aid: None,
        sid: tr.sid,
        select_sub: tr.select_sub,
        }
}

/// Send a command to the running mpv (blocking; used off the UI thread).
fn mpv_send(args: &[&str]) -> bool {
    use std::io::Write;
    let Ok(mut s) = std::os::unix::net::UnixStream::connect("/tmp/pifin-mpv.sock") else { return false };
    let cmd = serde_json::json!({ "command": args });
    s.write_all(format!("{cmd}\n").as_bytes()).is_ok()
}

/// External/extracted subtitles are added once playback runs: the server may take
/// a while to extract them from a big file, and that must not delay the picture.
/// The first extraction of an embedded subtitle can take minutes on a big file, longer
/// than mpv's network timeout, so the app downloads each one itself (long timeout) to a
/// local file and adds that; the URL is the fallback.
fn spawn_sub_loader(subs: Vec<(String, String)>, select: Option<usize>, exited: Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    if subs.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into();
        let tag = std::process::id();
        let mut sources = Vec::new();
        for (i, (url, title)) in subs.into_iter().enumerate() {
            if exited.load(Ordering::Relaxed) {
                return;
            }
            let path = format!("/tmp/pifin-sub-{tag}-{i}.{}", sub_ext(&url));
            let t0 = Instant::now();
            let got = agent.get(&url).call().map_err(|e| e.to_string()).and_then(|mut r| {
                r.body_mut().with_config().limit(32 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())
            });
            let src = match got.and_then(|b| std::fs::write(&path, b).map_err(|e| e.to_string())) {
                Ok(()) => {
                    eprintln!("SUB_FETCH {i} ok {}ms", t0.elapsed().as_millis());
                    path
                }
                Err(e) => {
                    eprintln!("SUB_FETCH {i} failed ({e}) {}", redact(&url));
                    url
                }
            };
            sources.push((src, title));
        }
        for _ in 0..60 {
            if exited.load(Ordering::Relaxed) || mpv_get("time-pos").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        for (i, (src, title)) in sources.into_iter().enumerate() {
            if exited.load(Ordering::Relaxed) {
                return;
            }
            mpv_send(&["sub-add", &src, if select == Some(i) { "select" } else { "auto" }, &title]);
        }
    });
}

/// File extension for a subtitle URL (`.../Stream.srt?...` -> "srt").
fn sub_ext(url: &str) -> &str {
    let path = url.split('?').next().unwrap_or(url);
    match path.rsplit_once('.') {
        Some((_, e)) if !e.is_empty() && !e.contains('/') => e,
        _ => "srt",
    }
}

/// A URL with its token hidden, for logs.
fn redact(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(p) = rest.find(['?', '&']) {
        out.push_str(&rest[..=p]);
        rest = &rest[p + 1..];
        let end = rest.find('&').unwrap_or(rest.len());
        let (param, tail) = rest.split_at(end);
        match param.split_once('=') {
            Some((k, _)) if k.eq_ignore_ascii_case("api_key") || k.eq_ignore_ascii_case("apikey") => {
                out.push_str(k);
                out.push_str("=REDACTED");
            }
            _ => out.push_str(param),
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Burn-in request for the default subtitle when it is image-based (PGS, DVD, DVB): those
/// cannot be added as text files, so the server draws them into the converted picture.
fn burn_in(streams: &[&jellyfin::MediaStream], defaults: Option<(Option<i32>, Option<i32>)>) -> String {
    let Some(i) = defaults.and_then(|d| d.1) else { return String::new() };
    match streams.iter().find(|s| s.kind == "Subtitle" && s.index == i) {
        Some(s) if !s.is_text_subtitle_stream => format!("&SubtitleStreamIndex={i}&SubtitleMethod=Encode"),
        _ => String::new(),
    }
}

/// Background reporter for one playback: Start, Progress every 10 s, then Stopped +
/// resume position once mpv is gone. Then asks the UI to refresh the home screen.
fn spawn_reporter(client: Arc<Client>, item: Item, session: Option<String>, exited: Arc<std::sync::atomic::AtomicBool>, weak: slint::Weak<App>) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let start = item.user_data.as_ref().map(|u| u.playback_position_ticks).unwrap_or(0);
        let run = item.run_time_ticks.unwrap_or(0);
        let mut pos = start;
        let ses = session.as_deref();
        let _ = client.report("start", &item.id, pos, false, ses);
        let mut last = Instant::now();
        while !exited.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            if let Some(p) = mpv_get("time-pos").and_then(|v| v.as_f64()) {
                pos = (p * 1e7) as i64;
            }
            if last.elapsed() >= Duration::from_secs(10) {
                last = Instant::now();
                let paused = mpv_get("pause").and_then(|v| v.as_bool()).unwrap_or(false);
                if let Err(e) = client.report("progress", &item.id, pos, paused, ses) {
                    eprintln!("{e}");
                }
            }
        }
        let _ = client.report("stop", &item.id, pos, false, ses);
        if let Some(s) = ses {
            client.stop_transcode(s);
        }
        if let Err(e) = client.save_position(&item.id, pos, run) {
            eprintln!("{e}");
        }
        eprintln!("SAVED id={} pos={}", item.id, fmt_ticks(pos));
        // refresh in place: no partial set (it would drop rows for a moment)
        fetch_home(&client, false, |rows| {
            save_home(&client.auth.user_id, &rows);
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                with_state(|state| {
                    let mut st = state.borrow_mut();
                    install_screen(&app, &mut st, rows, false, Some(0));
                });
            });
        });
    });
}

/// Wi-Fi state published by the comitup callback (/run/pifin/wifi.json).
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct WifiState {
    state: String,
    ap_ssid: String,
    ap_pass: String,
    ssid: String,
    failed: String,
    time: u64,
    // state PORTAL (pifin-portal): the Wi-Fi wants a browser login
    portal_ssid: String,
    portal_pass: String,
    mac: String,
    hotspot: bool,
    expired: bool,
}

/// What the TV shows on a hotel login page: title, steps, and the hotspot to put in
/// the QR code (none without a second radio: then the MAC is registered at reception).
fn portal_screen(w: &WifiState) -> (String, String, Option<(String, String)>) {
    let title = "This Wi-Fi needs a login".to_string();
    if w.hotspot && !w.portal_ssid.is_empty() {
        let text = format!(
            "\"{}\" shows a login page (terms, room number) before the internet works.\n\n\
             1.  Scan the code with your phone to join the network \"{}\" (password: {}).\n\n\
             2.  The hotel's page opens on your phone. Accept it or log in as you would for the phone.\n\n\
             3.  PiFin carries on by itself and the \"{}\" network closes.",
            w.ssid, w.portal_ssid, w.portal_pass, w.portal_ssid
        );
        return (title, text, Some((w.portal_ssid.clone(), w.portal_pass.clone())));
    }
    let mac = if w.mac.is_empty() { "unknown" } else { &w.mac };
    let first = if w.expired {
        "The PiFin-login network closed after 15 minutes without a login."
    } else {
        "PiFin can't open this page by itself, and a USB Wi-Fi adapter is needed to pass it on to your phone."
    };
    let text = format!(
        "\"{}\" shows a login page (terms, room number) before the internet works.\n\n\
         {first}\n\n\
         Ask reception to register this device address (MAC): {mac}\n\n\
         PiFin carries on by itself once it is registered.",
        w.ssid
    );
    (title, text, None)
}

const NOW_FILE: &str = "/tmp/pifin-now.json";
const APP_SOCK: &str = "/tmp/pifin-app.sock";

type RemoteFn = Box<dyn Fn(&App, &str, Option<Item>)>;
thread_local! {
    static REMOTE: RefCell<Option<RemoteFn>> = const { RefCell::new(None) };
}

/// The address phones on the same network use to reach us.
/// Set once the phone-remote hint was dismissed, so a late network doesn't bring it back.
static HINT_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn hide_remote_hint(app: &App) {
    HINT_OFF.store(true, std::sync::atomic::Ordering::Relaxed);
    app.set_remote_show(false);
}

fn local_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // documentation address: no packet is sent
    Some(s.local_addr().ok()?.ip())
}

/// The phone remote's address for the QR code: with the pairing token the scan pairs the
/// phone (D8); without one (no token file yet) the plain address.
fn remote_url(host: &str, token: Option<&str>) -> String {
    match token {
        Some(t) => format!("http://{host}:8080/pair?t={t}"),
        None => format!("http://{host}:8080"),
    }
}

/// The address phones reach the remote at; PIFIN_REMOTE_HOST overrides it (screenshots).
fn remote_host() -> Option<String> {
    std::env::var("PIFIN_REMOTE_HOST").ok().filter(|h| !h.is_empty()).or_else(|| local_ip().map(|ip| ip.to_string()))
}

/// A QR code as a 1-pixel-per-module image (scaled up by the UI, pixelated).
fn qr_image(text: &str) -> Option<slint::Image> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let n = code.width() as u32;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(n, n);
    for (px, c) in buf.make_mut_slice().iter_mut().zip(code.to_colors()) {
        let v = if c == qrcode::Color::Dark { 0 } else { 255 };
        *px = Rgba8Pixel { r: v, g: v, b: v, a: 255 };
    }
    Some(slint::Image::from_rgba8(buf))
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Standard Wi-Fi QR payload; phones join the network when they scan it.
fn wifi_qr(ssid: &str, pass: &str) -> Option<slint::Image> {
    let esc = |v: &str| v.chars().flat_map(|c| match c {
        '\\' | ';' | ',' | ':' | '"' => vec!['\\', c],
        _ => vec![c],
    }).collect::<String>();
    qr_image(&format!("WIFI:T:WPA;S:{};P:{};;", esc(ssid), esc(pass)))
}

/// Decide what the TV shows for a Wi-Fi state. Grace periods avoid flashing a
/// screen on the normal path (comitup passes through HOTSPOT at every boot and
/// a good connection takes ~5 s).
fn apply_wifi(app: &App, w: &WifiState, qr_cache: &mut Option<(String, slint::Image)>) -> bool {
    let age = now_s().saturating_sub(w.time);
    let showing = app.get_wifi_show();
    match w.state.as_str() {
        "HOTSPOT" if !w.failed.is_empty() || age >= 30 || showing => {
            let key = format!("{}\n{}", w.ap_ssid, w.ap_pass);
            if qr_cache.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
                *qr_cache = wifi_qr(&w.ap_ssid, &w.ap_pass).map(|i| (key, i));
            }
            app.set_wifi_title("Connect PiFin to Wi-Fi".into());
            app.set_wifi_text(format!(
                "1.  Scan the code with your phone to join the network \"{}\" (password: {}).\n\n\
                 2.  A page opens: pick your Wi-Fi and enter its password. If it doesn't open, go to http://10.41.0.1\n\n\
                 3.  PiFin connects and comes back here by itself.\n\n\
                 Or press OK to pick a network with the TV remote.",
                w.ap_ssid, w.ap_pass
            ).into());
            app.set_wifi_pickable(true);
            app.set_wifi_error(if w.failed.is_empty() { "".into() } else {
                format!("Couldn't connect to \"{}\". Check the password and try again.", w.failed).into()
            });
            if let Some((_, img)) = qr_cache {
                app.set_wifi_qr(img.clone());
                app.set_wifi_has_qr(true);
            }
            app.set_wifi_show(true);
            false
        }
        "CONNECTING" if age >= 8 || showing => {
            app.set_wifi_title(if w.ssid.is_empty() { "Connecting…".into() } else { format!("Connecting to \"{}\"…", w.ssid).into() });
            app.set_wifi_text("This takes a few seconds. If the setup screen comes back, the password was probably wrong.".into());
            app.set_wifi_pickable(false);
            app.set_wifi_error("".into());
            app.set_wifi_has_qr(false);
            app.set_wifi_show(true);
            false
        }
        "PORTAL" => {
            let (title, text, qr) = portal_screen(w);
            app.set_wifi_title(title.into());
            app.set_wifi_text(text.into());
            app.set_wifi_pickable(false);
            app.set_wifi_error("".into());
            app.set_wifi_has_qr(false);
            if let Some((ssid, pass)) = qr {
                let key = format!("{ssid}\n{pass}");
                if qr_cache.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
                    *qr_cache = wifi_qr(&ssid, &pass).map(|i| (key, i));
                }
                if let Some((_, img)) = qr_cache {
                    app.set_wifi_qr(img.clone());
                    app.set_wifi_has_qr(true);
                }
            }
            app.set_wifi_show(true);
            false
        }
        "CONNECTED" => {
            let was = showing;
            app.set_wifi_show(false);
            was
        }
        _ => false,
    }
}


/// JELLYFIN_URL from /etc/pifin/app.conf (world-readable, written by build.py).
fn server_url() -> Option<String> {
    let text = std::fs::read_to_string("/etc/pifin/app.conf").ok()?;
    text.lines().find_map(|l| {
        let v = l.strip_prefix("JELLYFIN_URL=")?.trim().trim_matches(|c| c == '\'' || c == '"');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// Sign a user in with Quick Connect: show the code full screen, wait for
/// approval, add the user to auth.json and restart as that user. `adding`:
/// other users exist, so Back returns to "Who's watching?".
fn sign_in(app: &App, adding: bool) {
    app.set_wifi_title("Sign in to Jellyfin".into());
    app.set_wifi_has_qr(false);
    app.set_wifi_show(true);
    if adding {
        app.on_cancel(|| profiles::restart(&[]));
    }
    // a new user joins the server the others are on; app.conf is for the first sign-in
    let Some(server) = profiles::Store::load().map(|s| s.server).filter(|s| !s.is_empty()).or_else(server_url) else {
        app.set_wifi_text("No server configured. Set JELLYFIN_URL in /etc/pifin/app.conf.".into());
        return;
    };
    app.set_wifi_text(format!("Connecting to {server}…").into());
    // one device id per user (Jellyfin ties a token to a device)
    let device_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default().trim().replace('-', "");
    let back = if adding { "\n\nBack to cancel." } else { "" };
    let weak = app.as_weak();
    std::thread::spawn(move || loop {
        let set = |title: String, text: String, err: String| {
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(a) = w.upgrade() {
                    if !title.is_empty() { a.set_wifi_title(title.into()); }
                    a.set_wifi_text(text.into());
                    a.set_wifi_error(err.into());
                }
            });
        };
        let qc = match jellyfin::QuickConnect::start(&server, &device_id) {
            Ok(q) => q,
            Err(e) => {
                set(String::new(), format!("Server: {server}{back}"), format!("{e}. Retrying…"));
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        eprintln!("QUICKCONNECT_CODE={}", qc.code);
        set(
            format!("Sign in code:  {}", qc.code),
            format!("On a phone or computer signed in to Jellyfin as the user to add, open Settings → Quick Connect and enter the code above.\n\nServer: {server}{back}"),
            String::new(),
        );
        // codes expire after a few minutes: ask for a new one then
        for _ in 0..100 {
            std::thread::sleep(Duration::from_secs(3));
            match qc.poll() {
                Ok(Some((auth, name))) => {
                    let mut store = profiles::Store::load().unwrap_or_default();
                    store.add(&auth, &name);
                    if let Err(e) = store.save() {
                        set(String::new(), String::new(), format!("Can't save the login: {e}"));
                        return;
                    }
                    eprintln!("signed in; restarting");
                    profiles::restart(&["--user", &auth.user_id]);
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    });
}

/// Avatar as a circle (alpha baked in, so the GPU draws a plain quad).
fn avatar(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    const S: u32 = 240;
    let img = image::load_from_memory(bytes).ok()?;
    let mut rgba = img.resize_to_fill(S, S, image::imageops::FilterType::Triangle).to_rgba8();
    let r = S as f32 / 2.0;
    for (x, y, px) in rgba.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - r, y as f32 + 0.5 - r);
        let a = (r - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
        px.0[3] = (px.0[3] as f32 * a) as u8;
    }
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), S, S))
}

fn initial(name: &str) -> String {
    name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
}

/// "Who's watching?". `current`: the user this process runs as (None at boot,
/// where Back does nothing). Picking another user restarts as that user.
fn show_profiles(app: &App, store: &profiles::Store, current: Option<String>) {
    let cards: Vec<Card> = store
        .users
        .iter()
        .map(|p| Card { id: p.user_id.clone().into(), title: p.name.clone().into(), subtitle: initial(&p.name).into(), ..Default::default() })
        .chain(std::iter::once(Card { id: "add".into(), title: "Add user".into(), ..Default::default() }))
        .collect();
    let focus = store.users.iter().position(|p| p.user_id == store.user_id).unwrap_or(0);
    let model = Rc::new(VecModel::from(cards));
    app.set_profiles(ModelRc::from(model.clone()));
    app.set_profile_focus(focus as i32);
    app.set_profiles_show(true);

    let weak = app.as_weak();
    {
        let current = current.clone();
        app.on_profile_pick(move |i| {
            let Some(app) = weak.upgrade() else { return };
            let Some(card) = app.get_profiles().row_data(i as usize) else { return };
            let id = card.id.to_string();
            if id == "add" {
                profiles::restart(&["--add-user"]);
            }
            if current.as_deref() == Some(id.as_str()) {
                app.set_profiles_show(false);
                return;
            }
            if let Some(mut st) = profiles::Store::load() {
                st.activate(&id);
                let _ = st.save();
            }
            profiles::restart(&["--user", &id]);
        });
    }
    let weak = app.as_weak();
    app.on_profile_close(move || {
        if current.is_some() {
            if let Some(app) = weak.upgrade() {
                app.set_profiles_show(false);
            }
        }
    });

    // names and avatars from the server, one user after another (a few small requests)
    let auths: Vec<Auth> = store.users.iter().map(|p| store.auth(p)).collect();
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let mut names = vec![];
        for (i, auth) in auths.into_iter().enumerate() {
            let Ok((name, bytes)) = Client::new(auth.clone()).me(240) else { continue };
            names.push((auth.user_id.clone(), name.clone()));
            let buf = bytes.as_deref().and_then(avatar);
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                let m = app.get_profiles();
                let Some(mut c) = m.row_data(i) else { return };
                c.subtitle = initial(&name).into();
                c.title = name.into();
                if let Some(b) = buf {
                    c.image = slint::Image::from_rgba8(b);
                    c.has_image = true;
                }
                m.set_row_data(i, c);
            });
        }
        // remember names changed on the server
        if let Some(mut st) = profiles::Store::load() {
            let mut changed = false;
            for (id, name) in names {
                if let Some(p) = st.users.iter_mut().find(|p| p.user_id == id && p.name != name) {
                    p.name = name;
                    changed = true;
                }
            }
            if changed {
                let _ = st.save();
            }
        }
    });
}

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("pifin {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let t0 = Instant::now();
    let app = App::new().expect("create window");
    app.window().set_fullscreen(true);
    // logo, embedded in the binary
    if let Ok(img) = image::load_from_memory(include_bytes!("../assets/logo.png")) {
        let rgba = img.to_rgba8();
        app.set_logo(slint::Image::from_rgba8(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height())));
    }
    // which user: --user <id> (after a pick), the only one, or ask
    let args: Vec<String> = std::env::args().collect();
    let store = profiles::Store::load();
    let Some(store) = store.filter(|_| !args.iter().any(|a| a == "--add-user")) else {
        let adding = profiles::Store::load().is_some();
        eprintln!("{}; starting Quick Connect sign-in", if adding { "adding a user" } else { "no saved login" });
        sign_in(&app, adding);
        app.run().ok();
        return;
    };
    let picked = args.iter().position(|a| a == "--user").and_then(|i| args.get(i + 1)).cloned();
    let profile = match picked {
        Some(id) => store.users.iter().find(|p| p.user_id == id).cloned(),
        None if store.users.len() == 1 => store.users.first().cloned(),
        None => None,
    };
    let Some(profile) = profile else {
        show_profiles(&app, &store, None);
        app.run().ok();
        return;
    };
    let auth = store.auth(&profile);
    // top bar: who is signed in (avatar from the disk cache, refreshed from the server)
    app.set_user_name(profile.name.clone().into());
    app.set_user_initial(initial(&profile.name).into());
    app.set_user_index(store.users.iter().position(|p| p.user_id == profile.user_id).unwrap_or(0) as i32);
    {
        let weak = app.as_weak();
        let auth = auth.clone();
        std::thread::spawn(move || {
            let key = format!("avatar-{}", auth.user_id);
            let bytes = img_cache::get(&key, || match Client::new(auth.clone()).me(96) {
                Ok((_, Some(b))) => Ok(b),
                Ok((_, None)) => Err("no avatar".into()),
                Err(e) => Err(e),
            });
            let Some(buf) = bytes.as_deref().and_then(avatar) else { return };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = weak.upgrade() {
                    app.set_user_image(slint::Image::from_rgba8(buf));
                    app.set_user_has_image(true);
                }
            });
        });
    }
    if store.user_id != profile.user_id {
        let mut st = store;
        st.activate(&profile.user_id);
        let _ = st.save();
    }
    let client = Arc::new(Client::new(auth.clone()));
    let (img_tx, img_rx) = mpsc::channel::<ImgJob>();

    let state = Rc::new(RefCell::new(State {
        screens: vec![],
        rows: Rc::new(VecModel::default()),
        next_id: 1,
        backdrop_gen: 0,
        img_tx: img_tx.clone(),
    }));
    app.set_rows(ModelRc::from(state.borrow().rows.clone()));
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    // ---- frame timing: render time per frame, reported every 5 s when PIFIN_FRAMELOG is set
    let frames: Rc<RefCell<(Option<Instant>, Vec<f64>)>> = Rc::new(RefCell::new((None, vec![])));
    {
        let frames = frames.clone();
        let mut first = true;
        let weak = app.as_weak();
        app.window()
            .set_rendering_notifier(move |state, _| {
                let mut f = frames.borrow_mut();
                match state {
                    slint::RenderingState::BeforeRendering => f.0 = Some(Instant::now()),
                    slint::RenderingState::AfterRendering => {
                        if let Some(st) = f.0.take() {
                            f.1.push(st.elapsed().as_secs_f64() * 1000.0);
                        }
                        if first {
                            first = false;
                            let size = weak.upgrade().map(|a| a.window().size()).unwrap_or_default();
                            eprintln!("FIRST_FRAME_MS={} window={}x{}", t0.elapsed().as_millis(), size.width, size.height);
                        }
                    }
                    _ => {}
                }
            })
            .ok();
    }
    let frame_timer = slint::Timer::default();
    if std::env::var_os("PIFIN_FRAMELOG").is_some() {
        let frames = frames.clone();
        frame_timer.start(slint::TimerMode::Repeated, Duration::from_secs(5), move || {
            let mut v = std::mem::take(&mut frames.borrow_mut().1);
            if v.is_empty() {
                return;
            }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
            eprintln!("FRAMES n={} p50={:.1}ms p95={:.1}ms max={:.1}ms", v.len(), p(0.5), p(0.95), v[v.len() - 1]);
        });
    }

    // ---- image workers -----------------------------------------------------
    std::thread::spawn(img_cache::prune);
    let img_rx = Arc::new(Mutex::new(img_rx));
    for _ in 0..2 {
        let rx = img_rx.clone();
        let client = client.clone();
        let weak = app.as_weak();
        std::thread::spawn(move || loop {
            let job = match rx.lock().unwrap().recv() {
                Ok(j) => j,
                Err(_) => return,
            };
            if let Slot::Backdrop { gen } = job.slot {
                if gen != BACKDROP_GEN.load(std::sync::atomic::Ordering::Relaxed) {
                    continue; // the focus moved on while this waited in the queue
                }
            }
            let key = format!("{}-{}-{}.jpg", job.item_id, job.kind, job.w);
            let Some(bytes) = img_cache::get(&key, || client.image(&job.item_id, job.kind, job.w)) else { continue };
            let Some(buf) = decode(&bytes, job.slot, job.w, job.h) else { continue };
            let slot = job.slot;
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                let img = slint::Image::from_rgba8(buf);
                with_state(|state| {
                    let st = state.borrow();
                    match slot {
                        Slot::Card { screen, row, col } => {
                            let Some(sc) = st.screens.iter().find(|s| s.id == screen) else { return };
                            if let Some(m) = sc.cards.get(row) {
                                if let Some(mut c) = m.row_data(col) {
                                    c.image = img;
                                    c.has_image = true;
                                    m.set_row_data(col, c);
                                }
                            }
                        }
                        Slot::Backdrop { gen } => {
                            if gen == st.backdrop_gen {
                                app.set_backdrop(img);
                                app.set_has_backdrop(true);
                            }
                        }
                    }
                });
            });
        });
    }

    // ---- home (API thread): retries every 15 s while the server is unreachable ----
    let load_home: Rc<dyn Fn()> = {
        let client = client.clone();
        let weak = app.as_weak();
        let loading = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Rc::new(move || {
            if loading.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let client = client.clone();
            let weak = weak.clone();
            let loading = loading.clone();
            if let Some(a) = weak.upgrade() {
                a.set_status("Loading…".into());
            }
            // first start: last session's home at once, refreshed in place below
            let mut cached = false;
            with_state(|state| {
                let mut st = state.borrow_mut();
                if st.screens.is_empty() {
                    if let (Some(rows), Some(app)) = (load_home_cache(&client.auth.user_id), weak.upgrade()) {
                        install_screen(&app, &mut st, rows, false, None);
                        cached = true;
                        eprintln!("HOME_CACHED_MS={}", t0.elapsed().as_millis());
                    }
                }
            });
            let weak2 = weak.clone();
            std::thread::spawn(move || {
                // unreachable: retry soon (at boot the network or Tailscale may come
                // up a few seconds after the app); some rows failed: a few more tries
                let (mut delay, mut partial_tries, mut shown) = (2, 0, cached);
                loop {
                    let mut got = false;
                    let mut last: Option<Rows> = None;
                    let ok = fetch_home(&client, !shown, |rows| {
                        got = true;
                        last = Some(rows.clone());
                        eprintln!("HOME_DATA_MS={} rows={}", t0.elapsed().as_millis(), rows.len());
                        let w = weak2.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(app) = w.upgrade() else { return };
                            with_state(|state| {
                                let mut st = state.borrow_mut();
                                let replace = if st.screens.is_empty() { None } else { Some(0) };
                                install_screen(&app, &mut st, rows, false, replace);
                            });
                            app.set_status("".into());
                            eprintln!("HOME_SHOWN_MS={}", t0.elapsed().as_millis());
                        });
                    });
                    shown |= got;
                    if let (true, Some(rows)) = (ok, &last) {
                        save_home(&client.auth.user_id, rows);
                    }
                    if ok || (got && partial_tries >= 3) {
                        break;
                    }
                    if got {
                        partial_tries += 1;
                    } else {
                        let w = weak2.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(a) = w.upgrade() {
                                a.set_status("Can't reach the server, retrying…".into());
                            }
                        });
                    }
                    // every 2 s for the first half minute (boot), then backing off
                    std::thread::sleep(Duration::from_secs(delay));
                    if t0.elapsed() > Duration::from_secs(30) {
                        delay = (delay * 2).min(15);
                    }
                }
                loading.store(false, std::sync::atomic::Ordering::SeqCst);
            });
        })
    };
    load_home();

    // ---- Wi-Fi screen: watch the callback's state file once per second ----------
    let wifi_timer = slint::Timer::default();
    let player: Rc<RefCell<Option<Child>>> = Rc::new(RefCell::new(None));
    {
        let weak = app.as_weak();
        let load_home = load_home.clone();
        let player = player.clone();
        let auth = auth.clone();
        // the address at startup is the baseline: only a change after start restarts us
        let mut seen_url = settings::current().system.jellyfin_url.clone();
        let mut qr_cache: Option<(String, slint::Image)> = None;
        let wifi_file = settings::paths().wifi; // PIFIN_WIFI_FILE overrides, for tests
        wifi_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
            let Some(app) = weak.upgrade() else { return };
            // the server address was changed (phone or TV): restart into the new one
            // (the daemon has already rewritten auth.json)
            let url = settings::current().system.jellyfin_url.clone();
            if server_changed(&mut seen_url, &url, &auth.server, player.borrow().is_none()) {
                eprintln!("SERVER address changed: restarting");
                profiles::restart(&["--user", &auth.user_id]);
            }
            let Ok(text) = std::fs::read_to_string(&wifi_file) else { return };
            let Ok(w) = serde_json::from_str::<WifiState>(&text) else { return };
            if apply_wifi(&app, &w, &mut qr_cache) {
                eprintln!("WIFI back: reloading home");
                load_home();
            }
        });
    }

    // ---- focus moves: clamp, remember column, update header + backdrop ---------
    let backdrop_timer = Rc::new(slint::Timer::default());
    {
        let weak = app.as_weak();
        let state = state.clone();
        let backdrop_timer = backdrop_timer.clone();
        app.on_moved(move |r, c| {
            let Some(app) = weak.upgrade() else { return };
            if app.get_remote_show() && app.get_cur_row() != r {
                hide_remote_hint(&app);
            }
            let mut st = state.borrow_mut();
            let st = &mut *st;
            let Some(sc) = st.screens.last_mut() else { return };
            if sc.items.is_empty() {
                return;
            }
            let r = r.clamp(0, sc.items.len() as i32 - 1) as usize;
            let col = if c < 0 { sc.focus[r] } else { c.clamp(0, sc.items[r].len() as i32 - 1) };
            sc.focus[r] = col;
            sc.cur_row = r as i32;
            if let Some(mut row) = st.rows.row_data(r) {
                if row.focus != col {
                    row.focus = col;
                    st.rows.set_row_data(r, row);
                }
            }
            app.set_cur_row(r as i32);
            let it = sc.items[r][col as usize].clone();
            if it.kind != "Person" {
                app.set_heading(it.title().into());
                app.set_info(info_line(&it).into());
                app.set_overview(it.overview.clone().unwrap_or_default().into());
            }
            // backdrop once the focus settles for 250 ms (no fetch storm while scrolling)
            if it.kind == "Person" {
                return;
            }
            st.backdrop_gen += 1;
            let gen = st.backdrop_gen;
            BACKDROP_GEN.store(gen, std::sync::atomic::Ordering::Relaxed);
            let id = backdrop_id(&it);
            let tx = st.img_tx.clone();
            backdrop_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(250), move || {
                let _ = tx.send(ImgJob { slot: Slot::Backdrop { gen }, item_id: id.clone(), kind: "Backdrop", w: BACKDROP_W, h: 720 });
            });
        });
    }

    // ---- open an item: series pages, or hand playback off to mpv -------------------
    // `play` = true plays episodes directly; false opens an episode's series page.
    let poll = Rc::new(slint::Timer::default());
    let start_gen = Rc::new(std::cell::Cell::new(0u64)); // one per Play, for the "slow" hint
    let open_item: Rc<dyn Fn(Item, bool)> = {
        let weak = app.as_weak();
        let player = player.clone();
        let start_gen = start_gen.clone();
        let poll = poll.clone();
        let auth = auth.clone();
        let client = client.clone();
        Rc::new(move |it: Item, play: bool| {
            if player.borrow().is_some() {
                return;
            }
            if let Some(a) = weak.upgrade() {
                hide_remote_hint(&a);
            }
            if it.kind == "Movie" && !play && it.action.is_none() {
                let client = client.clone();
                let weak = weak.clone();
                std::thread::spawn(move || {
                    let rows = fetch_movie(&client, &it.id);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        if !rows.is_empty() {
                            with_state(|state| install_screen(&app, &mut state.borrow_mut(), rows, false, None));
                        }
                    });
                });
                return;
            }
            if it.kind == "Person" {
                return;
            }
            // shows → seasons → episodes
            let page: Option<(Item, bool)> = match it.kind.as_str() {
                "Series" => Some((it.clone(), false)),
                "Season" => Some((it.clone(), true)),
                // "open" on an episode (phone): its season's episode list
                "Episode" if !play => it.season_id.clone().map(|sid| (Item {
                    id: sid, kind: "Season".into(), name: it.season_name.clone().unwrap_or_else(|| "Season".into()),
                    series_id: it.series_id.clone(), series_name: it.series_name.clone(), ..Default::default()
                }, true)),
                _ => None,
            };
            if let Some((target, is_season)) = page {
                let client = client.clone();
                let weak = weak.clone();
                if let Some(a) = weak.upgrade() {
                    a.set_status("Loading…".into());
                }
                std::thread::spawn(move || {
                    let rows = if is_season { fetch_season(&client, &target) } else { fetch_seasons(&client, &target.id) };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        app.set_status(if rows.is_empty() { "Nothing here".into() } else { "".into() });
                        if !rows.is_empty() {
                            // episode lists use the episodes' own stills and names
                            with_state(|state| install_screen(&app, &mut state.borrow_mut(), rows, is_season, None));
                        }
                    });
                });
                return;
            }
            if !matches!(it.kind.as_str(), "Movie" | "Episode") {
                return;
            }
            // feedback right away: the server lookup and mpv's buffering can take seconds
            if let Some(app) = weak.upgrade() {
                let shown = match (&it.series_name, it.parent_index_number, it.index_number) {
                    (Some(s), Some(se), Some(ep)) => format!("{s} · S{se}:E{ep} {}", it.name),
                    _ => it.name.clone(),
                };
                app.set_starting_title(shown.into());
                if !app.get_starting() {
                    app.set_starting_slow(false);
                    app.set_starting_note("".into());
                    app.set_starting(true);
                    start_gen.set(start_gen.get() + 1);
                    let (gen, start_gen, weak) = (start_gen.get(), start_gen.clone(), weak.clone());
                    slint::Timer::single_shot(Duration::from_secs(12), move || {
                        if let Some(a) = weak.upgrade().filter(|a| a.get_starting() && start_gen.get() == gen) {
                            a.set_starting_slow(true);
                        }
                    });
                }
            }
            // list items carry no media info: fetch the full item first, and measure
            // the link meanwhile (unless measured a minute ago), then come back here
            let need_link = link_bps().is_none();
            if it.media_sources.is_empty() || need_link || it.defaults.is_none() {
                let client = client.clone();
                let weak = weak.clone();
                let it = it.clone();
                std::thread::spawn(move || {
                    let probe = need_link.then(|| {
                        let c = client.clone();
                        std::thread::spawn(move || c.link_rate())
                    });
                    let defaults = it.defaults.is_none().then(|| {
                        let (c, id) = (client.clone(), it.id.clone());
                        std::thread::spawn(move || c.playback_defaults(&id))
                    });
                    let full = if it.media_sources.is_empty() { client.item(&it.id) } else { Ok(it) };
                    let full = full.map(|mut f| {
                        if let Some(d) = defaults {
                            // no answer: the old behaviour (mpv's / the file's own choice)
                            let d = d.join().ok().and_then(|r| r.map_err(|e| eprintln!("PLAYBACK_DEFAULTS: {e}")).ok());
                            // (-1, -1) matches no stream, so `tracks` leaves everything to mpv
                            f.defaults = Some(d.unwrap_or((Some(-1), Some(-1))));
                        }
                        f
                    });
                    if let Some(p) = probe {
                        let rate = p.join().ok().flatten().unwrap_or(0);
                        eprintln!("LINK_MBPS={:.1}", rate as f64 / 1e6);
                        if let Ok(mut l) = LINK.lock() {
                            *l = Some((Instant::now(), rate));
                        }
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        let Ok(full) = full else {
                            app.set_starting(false);
                            app.set_status("Couldn't load this item from the server".into());
                            return;
                        };
                        if !app.get_starting() {
                            return; // Back pressed while the server answered
                        }
                        REMOTE.with(|r| {
                            if let Some(f) = r.borrow().as_ref() {
                                f(&app, "play", Some(full));
                            }
                        });
                    });
                });
                return;
            }
            let lim = settings::effective(&settings::current(), settings::current_ssid().as_deref());
            let plan = plan(&auth, &it, link_bps().unwrap_or(0), &lim);
            if let Some(a) = weak.upgrade() {
                a.set_starting_note(plan.note.clone().into());
            }
            let start = it.user_data.as_ref().map(|u| u.playback_position_ticks as f64 / 1e7).unwrap_or(0.0);
            let title = match (&it.series_name, it.parent_index_number, it.index_number) {
                (Some(s), Some(se), Some(ep)) => format!("{s} - S{se}E{ep} - {}", it.name),
                _ => it.name.clone(),
            };
            // what the phone remote shows as "now playing"
            let now = serde_json::json!({
                "id": it.id, "name": it.name, "series": it.series_name, "season": it.parent_index_number,
                "episode": it.index_number, "year": it.production_year, "runTicks": it.run_time_ticks.unwrap_or(0),
                "imageId": poster_id(&it, false),
                "names": { "audio": plan.audio_names, "sub": plan.sub_names },
            });
            let _ = std::fs::write(NOW_FILE, now.to_string());
            // display mode for this video, switched off the UI thread while mpv starts
            let video = it.media_sources.first().and_then(|m| m.media_streams.iter().find(|s| s.kind == "Video"));
            let direct = plan.session.is_none();
            let video_h = video.and_then(|v| v.height).filter(|_| direct);
            let fps = video.and_then(|v| v.real_frame_rate.or(v.average_frame_rate));
            let hdr_hint = direct && lim.hdr && video.and_then(|v| v.video_range.as_deref()) == Some("HDR");
            let mode = std::cell::Cell::new(Some(modeswitch::switch_for(video_h, fps, lim.hdr, lim.max_height)));
            match mpv_command(&plan.url, start, &title, &MpvOpts { aid: plan.aid.clone(), sid: plan.sid.clone(), hdr_hint, ..MpvOpts::from_settings() }).spawn() {
                Ok(child) => {
                    eprintln!("MPV_START id={} t={}ms", it.id, t0.elapsed().as_millis());
                    *player.borrow_mut() = Some(child);
                    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    spawn_reporter(client.clone(), it.clone(), plan.session.clone(), exited.clone(), weak.clone());
                    let sub_titles = plan.sub_names.iter().skip(plan.sub_names.len() - plan.subs.len()).cloned();
                    spawn_sub_loader(plan.subs.iter().cloned().zip(sub_titles).collect(), plan.select_sub, exited.clone());
                    let weak = weak.clone();
                    let player = player.clone();
                    let poll_w = Rc::downgrade(&poll);
                    poll.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
                        let done = match player.borrow_mut().as_mut() {
                            Some(c) => matches!(c.try_wait(), Ok(Some(_)) | Err(_)),
                            None => true,
                        };
                        // mpv's window is up and covers the app: stop the spinner
                        if let Some(a) = weak.upgrade().filter(|a| a.get_starting()) {
                            if done || mpv_get("vo-configured").and_then(|v| v.as_bool()) == Some(true) {
                                eprintln!("MPV_VIDEO t={}ms", t0.elapsed().as_millis());
                                a.set_starting(false);
                            }
                        }
                        if done {
                            if let Some(t) = poll_w.upgrade() {
                                t.stop();
                            }
                            *player.borrow_mut() = None;
                            exited.store(true, std::sync::atomic::Ordering::Relaxed);
                            if let Some(h) = mode.take() {
                                modeswitch::restore(h);
                            }
                            let _ = std::fs::remove_file(NOW_FILE);
                            eprintln!("MPV_EXIT t={}ms", t0.elapsed().as_millis());
                            if let Some(a) = weak.upgrade() {
                                a.window().request_redraw();
                            }
                        }
                    });
                }
                Err(e) => {
                    if let Some(h) = mode.take() {
                        modeswitch::restore(h);
                    }
                    let _ = std::fs::remove_file(NOW_FILE);
                    eprintln!("mpv spawn failed: {e}");
                    if let Some(a) = weak.upgrade() {
                        a.set_starting(false);
                    }
                }
            }
        })
    };
    {
        let weak = app.as_weak();
        let player = player.clone();
        app.on_cancel_start(move || {
            if let Some(c) = player.borrow_mut().as_mut() {
                let _ = c.kill(); // the poll timer sees the exit and cleans up
            }
            if let Some(a) = weak.upgrade() {
                a.set_starting(false);
            }
            eprintln!("START_CANCELLED");
        });
    }
    {
        let state = state.clone();
        let open_item = open_item.clone();
        app.on_activate(move |r, c| {
            let it = {
                let st = state.borrow();
                let Some(it) = st.screens.last().and_then(|s| s.items.get(r as usize)).and_then(|v| v.get(c as usize)) else { return };
                it.clone()
            };
            // movies open their page; episodes and the page's Play cards play
            let play = it.kind != "Movie" || it.action.is_some();
            open_item(it, play);
        });
    }

    // ---- search: on-screen keyboard, results 0.4 s after the last key -------------------
    let kb: Vec<slint::SharedString> = ('a'..='z').chain('0'..='9').map(|c| c.to_string().into())
        .chain(["space", "delete", "clear"].iter().map(|k| (*k).into())).collect();
    app.set_kb_keys(ModelRc::new(VecModel::from(kb)));
    let search_timer = Rc::new(slint::Timer::default());
    let run_search: Rc<dyn Fn(String)> = {
        let weak = app.as_weak();
        let client = client.clone();
        let search_timer = search_timer.clone();
        Rc::new(move |q: String| {
            let weak = weak.clone();
            let client = client.clone();
            search_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(400), move || {
                let q = q.clone();
                let weak = weak.clone();
                let client = client.clone();
                if q.trim().len() < 2 {
                    return;
                }
                std::thread::spawn(move || {
                    let items = client.search(&q).unwrap_or_default();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        with_state(|state| {
                            let mut st = state.borrow_mut();
                            // only if the search screen is still on top with this query
                            let top = st.screens.len().wrapping_sub(1);
                            if st.screens.last().and_then(|s| s.search.as_deref()) != Some(q.as_str()) {
                                return;
                            }
                            let rows = if items.is_empty() { vec![] } else { vec![(format!("Results for \"{q}\""), items)] };
                            install_screen(&app, &mut st, rows, false, Some(top));
                        });
                    });
                });
            });
        })
    };
    let open_search: Rc<dyn Fn(String)> = {
        let weak = app.as_weak();
        let state = state.clone();
        let run_search = run_search.clone();
        Rc::new(move |q: String| {
            let Some(app) = weak.upgrade() else { return };
            hide_remote_hint(&app);
            let mut st = state.borrow_mut();
            let is_search = st.screens.last().map(|s| s.search.is_some()).unwrap_or(false);
            if !is_search {
                install_screen(&app, &mut st, vec![], false, None);
                app.set_kb_focus(0);
                app.set_kb_active(true);
            }
            if let Some(sc) = st.screens.last_mut() {
                sc.search = Some(q.clone());
            }
            show_top(&app, &mut st);
            drop(st);
            run_search(q);
        })
    };
    {
        let open_search = open_search.clone();
        app.on_open_search(move || open_search(String::new()));
    }
    {
        let state = state.clone();
        let weak = app.as_weak();
        let run_search = run_search.clone();
        app.on_kb_press(move |key| {
            let Some(app) = weak.upgrade() else { return };
            let mut st = state.borrow_mut();
            let Some(sc) = st.screens.last_mut() else { return };
            let Some(q) = sc.search.as_mut() else { return };
            match key.as_str() {
                "space" => q.push(' '),
                "delete" => { q.pop(); }
                "clear" => q.clear(),
                k => q.push_str(k),
            }
            let q = q.clone();
            app.set_query(q.clone().into());
            drop(st);
            run_search(q);
        });
    }

    // ---- commands from the phone remote: /tmp/pifin-app.sock, one line each ----------
    {
        let _ = std::fs::remove_file(APP_SOCK);
        match std::os::unix::net::UnixListener::bind(APP_SOCK) {
            Ok(listener) => {
                let weak = app.as_weak();
                let client = client.clone();
                std::thread::spawn(move || {
                    use std::io::BufRead;
                    for conn in listener.incoming().flatten() {
                        let mut line = String::new();
                        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                        if std::io::BufReader::new(conn).read_line(&mut line).is_err() {
                            continue;
                        }
                        let line = line.trim_end_matches(['\r', '\n']).to_string();
                        let mut parts = line.splitn(2, ' ');
                        let (mut cmd, arg) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                        if cmd == "text" {
                            cmd = line.clone();
                        }
                        // items are fetched here, off the UI thread
                        let item = if matches!(cmd.as_str(), "play" | "open") { client.item(&arg).ok() } else { None };
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(app) = weak.upgrade() else { return };
                            REMOTE.with(|r| {
                                if let Some(f) = r.borrow().as_ref() {
                                    f(&app, &cmd, item);
                                }
                            });
                        });
                    }
                });
            }
            Err(e) => eprintln!("command socket {APP_SOCK}: {e}"),
        }
        let state = state.clone();
        let open_item = open_item.clone();
        let open_search = open_search.clone();
        REMOTE.with(|r| {
            *r.borrow_mut() = Some(Box::new(move |app: &App, cmd: &str, item: Option<Item>| match cmd {
                "seen" => hide_remote_hint(app),
                "home" => {
                    let mut st = state.borrow_mut();
                    if st.screens.len() > 1 {
                        st.screens.truncate(1);
                        show_top(app, &mut st);
                    }
                }
                "play" | "open" => {
                    if let Some(it) = item {
                        open_item(it, cmd == "play");
                    }
                }
                c if c.starts_with("text ") => open_search(c[5..].to_string()),
                _ => eprintln!("unknown remote command {cmd}"),
            }));
        });
    }

    // ---- boot hint: QR code for the phone remote until a key press or a phone connects.
    // The app starts before the network, so the address is looked up every 2 s
    // (up to 3 min) until there is one, unless the hint was dismissed meanwhile.
    if std::env::var_os("PIFIN_RESTARTED").is_none() {
        let weak = app.as_weak();
        let tries = std::cell::Cell::new(0u32);
        let show = move || -> bool {
            let Some(app) = weak.upgrade() else { return true };
            tries.set(tries.get() + 1);
            if HINT_OFF.load(std::sync::atomic::Ordering::Relaxed) || tries.get() > 90 {
                return true;
            }
            let Some(host) = remote_host() else { return false };
            // the token is read again on every try: a Re-pair may have replaced it
            let token = settings::pair_token();
            if let Some(img) = qr_image(&remote_url(&host, token.as_deref())) {
                app.set_remote_qr(img);
                // the text shows the address only; the token stays in the QR code
                app.set_remote_url(remote_url(&host, None).into());
                app.set_remote_show(true);
            }
            true
        };
        if !show() {
            let timer = Rc::new(slint::Timer::default());
            let t = timer.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_secs(2), move || {
                if show() {
                    t.stop();
                }
            });
        }
    }

    // ---- Back: pop a screen; at home it opens "Who's watching?" (PIFIN_EXIT quits, for probes)
    {
        let weak = app.as_weak();
        let state = state.clone();
        let me = auth.user_id.clone();
        app.on_back(move || {
            let Some(app) = weak.upgrade() else { return };
            if app.get_remote_show() {
                hide_remote_hint(&app);
                return;
            }
            let mut st = state.borrow_mut();
            if st.screens.len() > 1 {
                st.screens.pop();
                show_top(&app, &mut st);
            } else if std::env::var_os("PIFIN_EXIT").is_some() {
                let _ = slint::quit_event_loop();
            } else if let Some(store) = profiles::Store::load() {
                drop(st);
                show_profiles(&app, &store, Some(me.clone()));
            }
        });
    }
    {
        // Test connection's server side; its result also serves the next Play
        let probe_client = client.clone();
        settings_ui::install(&app, Arc::new(move || {
            let rate = probe_client.server_test();
            if let (Some(r), Ok(mut l)) = (rate, LINK.lock()) {
                *l = Some((Instant::now(), r));
            }
            rate
        }), client.clone(), profile.name.clone());
    }
    {
        let weak = app.as_weak();
        app.on_open_settings(move || {
            let Some(app) = weak.upgrade() else { return };
            hide_remote_hint(&app);
            settings_ui::open(&app);
        });
    }
    {
        let weak = app.as_weak();
        let me = auth.user_id.clone();
        app.on_open_profiles(move || {
            let Some(app) = weak.upgrade() else { return };
            hide_remote_hint(&app);
            if let Some(store) = profiles::Store::load() {
                show_profiles(&app, &store, Some(me.clone()));
            }
        });
    }

    app.on_unknown_key(|t| {
        let cps: Vec<String> = t.chars().map(|c| format!("U+{:04X}", c as u32)).collect();
        eprintln!("UNKNOWN_KEY {}", cps.join(" "));
    });

    app.run().expect("event loop");
}

#[cfg(test)]
mod tests {
    use super::*;
    use jellyfin::MediaSource;

    fn item(bitrate: i64) -> Item {
        Item { id: "i1".into(), media_sources: vec![MediaSource { id: "m1".into(), bitrate: Some(bitrate), ..Default::default() }], ..Default::default() }
    }
    fn auth() -> Auth {
        Auth { server: "http://jf:8096".into(), user_id: "u".into(), token: "t".into(), device_id: "d".into() }
    }

    #[test]
    fn budget_follows_the_link() {
        assert_eq!(bitrate_budget(0), (MAX_DIRECT_BITRATE, MAX_TRANSCODE_BITRATE)); // unknown: as before
        assert_eq!(bitrate_budget(7_400_000), (5_920_000, 4_440_000));
        assert_eq!(bitrate_budget(100_000_000), (MAX_DIRECT_BITRATE, MAX_TRANSCODE_BITRATE));
        assert_eq!(bitrate_budget(1_000_000), (800_000, 1_500_000)); // floor for converted streams
    }

    #[test]
    fn weak_link_converts_to_a_stream_it_can_carry() {
        // the LOTR case: 10 Mbit/s conversion on a ~7 Mbit/s Wi-Fi froze every 10-20 s
        let p = plan(&auth(), &item(30_000_000), 7_400_000, &none());
        assert!(p.url.contains("master.m3u8") && p.url.contains("VideoBitrate=4184000") && p.url.contains("MaxWidth=1920"), "{}", p.url);
        assert!(!p.note.is_empty());
        // very weak: 720p
        let p = plan(&auth(), &item(30_000_000), 4_000_000, &none());
        assert!(p.url.contains("VideoBitrate=2144000") && p.url.contains("MaxWidth=1280"), "{}", p.url);
        // a light file still plays directly; one too heavy for this link is converted
        assert!(plan(&auth(), &item(5_000_000), 7_400_000, &none()).url.contains("static=true"));
        assert!(plan(&auth(), &item(7_000_000), 7_400_000, &none()).url.contains("master.m3u8"));
        // unknown link: the old rule (direct up to 16 Mbit/s, else 10 Mbit/s, no note)
        assert!(plan(&auth(), &item(12_000_000), 0, &none()).url.contains("static=true"));
        let p = plan(&auth(), &item(30_000_000), 0, &none());
        assert!(p.url.contains("VideoBitrate=9616000") && p.note.is_empty(), "{}", p.url);
    }

    #[test]
    fn portal_state_maps_to_the_login_screen() {
        // the JSON pifin-portal writes, comitup's fields kept
        let w: WifiState = serde_json::from_str(
            r#"{"state":"PORTAL","ssid":"Hotel","ap_ssid":"PiFin-setup","ap_pass":"x","time":1,
                "portal_ssid":"PiFin-login","portal_pass":"secret12","mac":"00:c0:ca:00:00:02",
                "hotspot":true,"expired":false}"#).unwrap();
        let (title, text, qr) = portal_screen(&w);
        assert_eq!(title, "This Wi-Fi needs a login");
        assert_eq!(qr, Some(("PiFin-login".to_string(), "secret12".to_string())));
        assert!(text.contains("\"Hotel\"") && text.contains("PiFin carries on by itself"), "{text}");
        assert!(!text.contains("00:c0:ca"), "{text}");
        // no adapter: no QR, the MAC to register at reception
        let w = WifiState { hotspot: false, portal_ssid: String::new(), portal_pass: String::new(), ..w };
        let (_, text, qr) = portal_screen(&w);
        assert_eq!(qr, None);
        assert!(text.contains("reception") && text.contains("00:c0:ca:00:00:02"), "{text}");
        assert!(!text.contains("closed after 15 minutes"), "{text}");
        // the hotspot timed out: MAC screen, saying why
        let (_, text, qr) = portal_screen(&WifiState { expired: true, ..w });
        assert_eq!(qr, None);
        assert!(text.contains("closed after 15 minutes") && text.contains("00:c0:ca:00:00:02"), "{text}");
    }

    #[test]
    fn server_change_restarts_once_per_new_url() {
        let mut seen = "http://a:8096".to_string();
        // same address as at startup (even with a mismatching auth.json): no restart
        assert!(!server_changed(&mut seen, "http://a:8096/", "http://old:8096", true));
        // changed while a player runs: wait
        assert!(!server_changed(&mut seen, "http://b:8096", "http://a:8096", false));
        // changed and idle: restart once
        assert!(server_changed(&mut seen, "http://b:8096", "http://a:8096", true));
        // the daemon never rewrote auth.json: still the same url, no loop
        assert!(!server_changed(&mut seen, "http://b:8096", "http://a:8096", true));
        // another new value fires again; one already applied does not
        assert!(server_changed(&mut seen, "http://c:8096", "http://a:8096", true));
        assert!(!server_changed(&mut seen, "http://c:8096", "http://c:8096/", true));
    }


    fn none() -> settings::Playback {
        settings::Playback::default()
    }

    #[test]
    fn decide_applies_the_users_limits() {
        let lim = |max_bitrate, max_height, prefer_original| settings::Playback { max_bitrate, max_height, hdr: false, prefer_original };
        // a 6 Mbit/s cap on a fast link converts an 8 Mbit/s file at 6 Mbit/s
        assert_eq!(decide(8_000_000, None, 50_000_000, &lim(6_000_000, 1080, true)), Decision::Convert { target: 6_000_000, max_height: 1080 });
        // a 2160p file under a 1080 cap is converted, however light
        assert_eq!(decide(5_000_000, Some(2160), 0, &lim(0, 1080, true)), Decision::Convert { target: MAX_TRANSCODE_BITRATE, max_height: 1080 });
        assert_eq!(decide(5_000_000, Some(1080), 0, &lim(0, 1080, true)), Decision::Direct);
        // prefer original off: even a 2 Mbit/s file is converted
        assert!(matches!(decide(2_000_000, None, 0, &lim(0, 1080, false)), Decision::Convert { .. }));
        // no limits: the link-only rules, as in weak_link_converts_to_a_stream_it_can_carry
        assert_eq!(decide(30_000_000, None, 7_400_000, &none()), Decision::Convert { target: 4_440_000, max_height: 1080 });
        assert_eq!(decide(5_000_000, None, 7_400_000, &none()), Decision::Direct);
        assert_eq!(decide(7_000_000, None, 7_400_000, &none()), Decision::Convert { target: 4_440_000, max_height: 1080 });
        assert_eq!(decide(12_000_000, None, 0, &none()), Decision::Direct);
    }

    #[test]
    fn plan_caps_the_converted_picture_to_max_height() {
        let lim = settings::Playback { max_height: 720, ..none() };
        let p = plan(&auth(), &item(30_000_000), 0, &lim);
        assert!(p.url.contains("MaxHeight=720&") && p.url.contains("MaxWidth=1280"), "{}", p.url);
        // only the user's cap caused this: no "lower quality for this connection" note
        assert!(p.note.is_empty());
        assert_eq!([2160, 1080, 720, 480].map(width_for_height), [3840, 1920, 1280, 854]);
    }

    #[test]
    fn mpv_gets_subtitle_scale_and_passthrough_only_when_set() {
        let args = |o: &MpvOpts| mpv_command("http://x/v", 0.0, "t", o).get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let plain = args(&MpvOpts { sub_scale: 1.0, passthrough: false, aid: None, sid: None, hdr_hint: false });
        assert!(!plain.iter().any(|a| a.starts_with("--sub-scale") || a.starts_with("--audio-spdif")), "{plain:?}");
        let set = args(&MpvOpts { sub_scale: 1.25, passthrough: true, aid: Some("2".into()), sid: Some("3".into()), hdr_hint: false });
        for want in ["--sub-scale=1.25", "--audio-spdif=ac3,eac3,dts,dts-hd,truehd", "--aid=2", "--sid=3"] {
            assert!(set.iter().any(|a| a == want), "{want} missing in {set:?}");
        }
    }

    #[test]
    fn default_tracks_become_mpv_track_numbers() {
        let st = |index, kind: &str, ext: bool| jellyfin::MediaStream {
            index,
            kind: kind.into(),
            is_external: ext,
            is_text_subtitle_stream: kind == "Subtitle",
            ..Default::default()
        };
        // V0, A1 eng, A2 fre, S3 eng, S4 fre, external S5
        let all = [st(0, "Video", false), st(1, "Audio", false), st(2, "Audio", false), st(3, "Subtitle", false), st(4, "Subtitle", false), st(5, "Subtitle", true)];
        let s: Vec<&jellyfin::MediaStream> = all.iter().collect();
        let t = tracks(&s, Some((Some(2), Some(4))), false);
        assert_eq!((t.aid.as_deref(), t.sid.as_deref(), t.select_sub), (Some("2"), Some("2"), None));
        // an external default subtitle is selected when it is added
        let t = tracks(&s, Some((Some(1), Some(5))), false);
        assert_eq!((t.aid.as_deref(), t.sid, t.select_sub), (Some("1"), None, Some(0)));
        // no default subtitle: off
        assert_eq!(tracks(&s, Some((Some(1), None)), false).sid.as_deref(), Some("no"));
        // not found, or not asked: leave mpv's choice
        assert_eq!(tracks(&s, Some((Some(-1), Some(-1))), false), Tracks::default());
        assert_eq!(tracks(&s, None, false), Tracks::default());
        // converted: every text subtitle is a file, the default one is selected
        let t = tracks(&s, Some((Some(2), Some(4))), true);
        assert_eq!((t.aid, t.sid, t.select_sub), (None, None, Some(1)));
    }

    #[test]
    fn converted_stream_burns_in_only_an_image_default_subtitle() {
        let st = |index, text: bool| jellyfin::MediaStream { index, kind: "Subtitle".into(), is_text_subtitle_stream: text, ..Default::default() };
        // like the Vikings file: S2, S3 subrip; S4, S5 PGS
        let all = [st(2, true), st(3, true), st(4, false), st(5, false)];
        let s: Vec<&jellyfin::MediaStream> = all.iter().collect();
        assert_eq!(burn_in(&s, Some((Some(1), Some(2)))), "");
        assert_eq!(burn_in(&s, Some((Some(1), Some(5)))), "&SubtitleStreamIndex=5&SubtitleMethod=Encode");
        assert_eq!(burn_in(&s, Some((Some(1), None))), "");
        assert_eq!(burn_in(&s, None), "");
        // converted: text subtitles become files numbered in order; the default is selected
        let t = tracks(&s, Some((Some(1), Some(3))), true);
        assert_eq!((t.sid, t.select_sub), (None, Some(1)));
        // an image default is burnt in, no text file selected
        assert_eq!(tracks(&s, Some((Some(1), Some(4))), true).select_sub, None);
    }

    #[test]
    fn subtitle_urls_lose_their_token_in_logs() {
        let u = "http://s:8096/Videos/x/y/Subtitles/2/0/Stream.srt?ApiKey=SECRET&a=1&api_key=SECRET2";
        let r = redact(u);
        assert!(!r.contains("SECRET"), "{r}");
        assert_eq!(r, "http://s:8096/Videos/x/y/Subtitles/2/0/Stream.srt?ApiKey=REDACTED&a=1&api_key=REDACTED");
        assert_eq!(redact("http://s/a?b=1"), "http://s/a?b=1");
        assert_eq!(sub_ext(u), "srt");
        assert_eq!(sub_ext("http://s/Stream.vtt?api_key=1.2"), "vtt");
        assert_eq!(sub_ext("http://s.lan/x"), "srt");
    }

    #[test]
    fn remote_qr_carries_the_pairing_token() {
        let t = "0123456789abcdef0123456789abcdef";
        assert_eq!(remote_url("192.0.2.5", Some(t)), format!("http://192.0.2.5:8080/pair?t={t}"));
        assert_eq!(remote_url("192.0.2.5", None), "http://192.0.2.5:8080");
    }

}
