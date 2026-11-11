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

/// mpv arguments proven on the Pi 4 (0 dropped frames on HEVC 10-bit 1080p).
fn mpv_command(url: &str, start_s: f64, title: &str) -> Command {
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
    c.arg(url);
    c
}

fn load_auth() -> Result<Auth, String> {
    let path = auth_path();
    let s = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    serde_json::from_str(&s).map_err(|e| format!("{path}: {e}"))
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
fn fetch_home(client: &Client) -> Vec<(String, Vec<Item>)> {
    let mut rows = vec![];
    let mut push = |title: String, r: Result<Vec<Item>, String>| match r {
        Ok(v) if !v.is_empty() => rows.push((title, v)),
        Ok(_) => {}
        Err(e) => eprintln!("{e}"),
    };
    push("Continue Watching".into(), client.resume());
    push("Next Up".into(), client.next_up());
    if let Ok(views) = client.views() {
        for v in views.iter() {
            match v.collection_type.as_deref() {
                Some("movies") => push(format!("Latest {}", v.name), client.latest(&v.id)),
                Some("tvshows") => push(v.name.clone(), client.shows(&v.id)),
                _ => {}
            }
        }
    }
    rows
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
            let old = &st.screens[i];
            screen.cur_row = old.cur_row.min(screen.items.len() as i32 - 1).max(0);
            for (r, f) in screen.focus.iter_mut().enumerate() {
                if let Some(of) = old.focus.get(r) {
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

/// How to play an item: the URL, extra subtitle files, and the track names the
/// phone remote shows (in mpv's order).
struct Plan {
    url: String,
    subs: Vec<String>,
    session: Option<String>,
    audio_names: Vec<String>,
    sub_names: Vec<String>,
}

fn plan(auth: &Auth, it: &Item) -> Plan {
    let server = auth.server.trim_end_matches('/');
    let token = &auth.token;
    let ms = it.media_sources.first();
    let msid = ms.map(|m| m.id.clone()).unwrap_or_else(|| it.id.clone());
    let streams: Vec<&jellyfin::MediaStream> = ms.map(|m| m.media_streams.iter().collect()).unwrap_or_default();
    let name = |s: &jellyfin::MediaStream| s.display_title.clone().or(s.title.clone()).unwrap_or_else(|| format!("Track {}", s.index));
    let sub_url = |s: &jellyfin::MediaStream| format!("{server}/Videos/{}/{msid}/Subtitles/{}/0/Stream.srt?ApiKey={token}", it.id, s.index);
    let bitrate = ms.and_then(|m| m.bitrate).unwrap_or(0);
    if bitrate <= MAX_DIRECT_BITRATE {
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
        };
    }
    // converted by the server (hardware encoder on the server side), HLS so seeking works
    let session = std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default().trim().replace('-', "");
    let audio = streams.iter().find(|s| s.kind == "Audio" && s.is_default).or(streams.iter().find(|s| s.kind == "Audio"));
    let text_subs: Vec<&&jellyfin::MediaStream> = streams.iter().filter(|s| s.kind == "Subtitle" && s.is_text_subtitle_stream).collect();
    eprintln!("TRANSCODE id={} source_bitrate={}", it.id, bitrate);
    Plan {
        url: format!(
            "{server}/Videos/{}/master.m3u8?MediaSourceId={msid}&DeviceId={}&PlaySessionId={session}&ApiKey={token}\
             &VideoCodec=h264&AudioCodec=aac&VideoBitrate=10000000&AudioBitrate=384000&MaxAudioChannels=6\
             &TranscodingMaxAudioChannels=6&MaxWidth=1920&SegmentContainer=ts&MinSegments=1&BreakOnNonKeyFrames=True{}",
            it.id, auth.device_id,
            audio.map(|a| format!("&AudioStreamIndex={}", a.index)).unwrap_or_default()
        ),
        subs: text_subs.iter().map(|s| sub_url(s)).collect(),
        session: Some(session),
        audio_names: audio.map(|a| vec![format!("{} (converted)", name(a))]).unwrap_or_default(),
        sub_names: text_subs.iter().map(|s| name(s)).collect(),
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
fn spawn_sub_loader(subs: Vec<(String, String)>) {
    if subs.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..60 {
            if mpv_get("time-pos").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        for (url, title) in subs {
            mpv_send(&["sub-add", &url, "auto", &title]);
        }
    });
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
        let rows = fetch_home(&client);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            with_state(|state| {
                let mut st = state.borrow_mut();
                install_screen(&app, &mut st, rows, false, Some(0));
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
}

const WIFI_FILE: &str = "/run/pifin/wifi.json";
const NOW_FILE: &str = "/tmp/pifin-now.json";
const APP_SOCK: &str = "/tmp/pifin-app.sock";

type RemoteFn = Box<dyn Fn(&App, &str, Option<Item>)>;
thread_local! {
    static REMOTE: RefCell<Option<RemoteFn>> = const { RefCell::new(None) };
}

/// The address phones on the same network use to reach us.
fn local_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // documentation address: no packet is sent
    Some(s.local_addr().ok()?.ip())
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
                 3.  PiFin connects and comes back here by itself.",
                w.ap_ssid, w.ap_pass
            ).into());
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
            app.set_wifi_error("".into());
            app.set_wifi_has_qr(false);
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


fn auth_path() -> String {
    std::env::var("PIFIN_AUTH").unwrap_or_else(|_| {
        format!("{}/.config/pifin/auth.json", std::env::var("HOME").unwrap_or_default())
    })
}

/// JELLYFIN_URL from /etc/pifin/app.conf (world-readable, written by build.py).
fn server_url() -> Option<String> {
    let text = std::fs::read_to_string("/etc/pifin/app.conf").ok()?;
    text.lines().find_map(|l| {
        let v = l.strip_prefix("JELLYFIN_URL=")?.trim().trim_matches(|c| c == '\'' || c == '"');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// No saved login: show a Quick Connect code full screen, wait for approval,
/// save the login (0600) and restart into the home screen.
fn sign_in(app: &App) {
    app.set_wifi_title("Sign in to Jellyfin".into());
    app.set_wifi_has_qr(false);
    app.set_wifi_show(true);
    let Some(server) = server_url() else {
        app.set_wifi_text("No server configured. Set JELLYFIN_URL in /etc/pifin/app.conf.".into());
        return;
    };
    app.set_wifi_text(format!("Connecting to {server}…").into());
    let device_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid").unwrap_or_default().trim().replace('-', "");
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
                set(String::new(), format!("Server: {server}"), format!("{e}. Retrying…"));
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        eprintln!("QUICKCONNECT_CODE={}", qc.code);
        set(
            format!("Sign in code:  {}", qc.code),
            format!("On a phone or computer signed in to Jellyfin, open Settings → Quick Connect and enter the code above.\n\nServer: {server}"),
            String::new(),
        );
        // codes expire after a few minutes: ask for a new one then
        for _ in 0..100 {
            std::thread::sleep(Duration::from_secs(3));
            match qc.poll() {
                Ok(Some(auth)) => {
                    let path = auth_path();
                    if let Some(dir) = std::path::Path::new(&path).parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    use std::os::unix::fs::OpenOptionsExt;
                    let saved = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)
                        .and_then(|mut f| std::io::Write::write_all(&mut f, serde_json::to_string(&auth).unwrap_or_default().as_bytes()));
                    if let Err(e) = saved {
                        set(String::new(), String::new(), format!("Can't save the login: {e}"));
                        return;
                    }
                    eprintln!("signed in; restarting");
                    use std::os::unix::process::CommandExt;
                    let err = Command::new(std::env::current_exe().unwrap_or_default()).exec();
                    eprintln!("restart failed: {err}");
                    std::process::exit(1);
                }
                Ok(None) => {}
                Err(_) => break,
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
    // logo, embedded in the binary
    if let Ok(img) = image::load_from_memory(include_bytes!("../assets/logo.png")) {
        let rgba = img.to_rgba8();
        app.set_logo(slint::Image::from_rgba8(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height())));
    }
    let auth = match load_auth() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("auth: {e}; starting Quick Connect sign-in");
            sign_in(&app);
            app.run().ok();
            return;
        }
    };
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
                            eprintln!("FIRST_FRAME_MS={}", t0.elapsed().as_millis());
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
            let Ok(bytes) = client.image(&job.item_id, job.kind, job.w) else { continue };
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
            let weak2 = weak.clone();
            std::thread::spawn(move || {
                let rows = loop {
                    let rows = fetch_home(&client);
                    if !rows.is_empty() {
                        break rows;
                    }
                    let w = weak2.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(a) = w.upgrade() {
                            a.set_status("Can't reach the server, retrying…".into());
                        }
                    });
                    std::thread::sleep(Duration::from_secs(15));
                };
                eprintln!("HOME_DATA_MS={} rows={}", t0.elapsed().as_millis(), rows.len());
                loading.store(false, std::sync::atomic::Ordering::SeqCst);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = weak2.upgrade() else { return };
                    with_state(|state| {
                        let mut st = state.borrow_mut();
                        let replace = if st.screens.is_empty() { None } else { Some(0) };
                        install_screen(&app, &mut st, rows, false, replace);
                    });
                    app.set_status("".into());
                    eprintln!("HOME_SHOWN_MS={}", t0.elapsed().as_millis());
                });
            });
        })
    };
    load_home();

    // ---- Wi-Fi screen: watch the callback's state file once per second ----------
    let wifi_timer = slint::Timer::default();
    {
        let weak = app.as_weak();
        let load_home = load_home.clone();
        let mut qr_cache: Option<(String, slint::Image)> = None;
        wifi_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
            let Some(app) = weak.upgrade() else { return };
            let Ok(text) = std::fs::read_to_string(WIFI_FILE) else { return };
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
                app.set_remote_show(false);
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
            let id = backdrop_id(&it);
            let tx = st.img_tx.clone();
            backdrop_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(250), move || {
                let _ = tx.send(ImgJob { slot: Slot::Backdrop { gen }, item_id: id.clone(), kind: "Backdrop", w: BACKDROP_W, h: 720 });
            });
        });
    }

    // ---- open an item: series pages, or hand playback off to mpv -------------------
    // `play` = true plays episodes directly; false opens an episode's series page.
    let player: Rc<RefCell<Option<Child>>> = Rc::new(RefCell::new(None));
    let poll = Rc::new(slint::Timer::default());
    let open_item: Rc<dyn Fn(Item, bool)> = {
        let weak = app.as_weak();
        let player = player.clone();
        let poll = poll.clone();
        let auth = auth.clone();
        let client = client.clone();
        Rc::new(move |it: Item, play: bool| {
            if player.borrow().is_some() {
                return;
            }
            if let Some(a) = weak.upgrade() {
                a.set_remote_show(false);
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
            // list items carry no media info: fetch the full item first, then come back here
            if it.media_sources.is_empty() {
                let client = client.clone();
                let weak = weak.clone();
                let id = it.id.clone();
                std::thread::spawn(move || {
                    let Ok(full) = client.item(&id) else { return };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        REMOTE.with(|r| {
                            if let Some(f) = r.borrow().as_ref() {
                                f(&app, "play", Some(full));
                            }
                        });
                    });
                });
                return;
            }
            let plan = plan(&auth, &it);
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
            match mpv_command(&plan.url, start, &title).spawn() {
                Ok(child) => {
                    eprintln!("MPV_START id={} t={}ms", it.id, t0.elapsed().as_millis());
                    *player.borrow_mut() = Some(child);
                    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    spawn_reporter(client.clone(), it.clone(), plan.session.clone(), exited.clone(), weak.clone());
                    let sub_titles = plan.sub_names.iter().skip(plan.sub_names.len() - plan.subs.len()).cloned();
                    spawn_sub_loader(plan.subs.iter().cloned().zip(sub_titles).collect());
                    let weak = weak.clone();
                    let player = player.clone();
                    let poll_w = Rc::downgrade(&poll);
                    poll.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
                        let done = match player.borrow_mut().as_mut() {
                            Some(c) => matches!(c.try_wait(), Ok(Some(_)) | Err(_)),
                            None => true,
                        };
                        if done {
                            if let Some(t) = poll_w.upgrade() {
                                t.stop();
                            }
                            *player.borrow_mut() = None;
                            exited.store(true, std::sync::atomic::Ordering::Relaxed);
                            let _ = std::fs::remove_file(NOW_FILE);
                            eprintln!("MPV_EXIT t={}ms", t0.elapsed().as_millis());
                            if let Some(a) = weak.upgrade() {
                                a.window().request_redraw();
                            }
                        }
                    });
                }
                Err(e) => {
                    let _ = std::fs::remove_file(NOW_FILE);
                    eprintln!("mpv spawn failed: {e}");
                }
            }
        })
    };
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
            app.set_remote_show(false);
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
                "seen" => app.set_remote_show(false),
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

    // ---- boot hint: QR code for the phone remote until a key press or a phone connects
    if let Some(ip) = local_ip() {
        let url = format!("http://{ip}:8080");
        if let Some(img) = qr_image(&url) {
            app.set_remote_qr(img);
            app.set_remote_url(url.into());
            app.set_remote_show(true);
        }
    }

    // ---- Back: pop a screen; at home it does nothing (unless PIFIN_EXIT for probes)
    {
        let weak = app.as_weak();
        let state = state.clone();
        app.on_back(move || {
            let Some(app) = weak.upgrade() else { return };
            if app.get_remote_show() {
                app.set_remote_show(false);
                return;
            }
            let mut st = state.borrow_mut();
            if st.screens.len() > 1 {
                st.screens.pop();
                show_top(&app, &mut st);
            } else if std::env::var_os("PIFIN_EXIT").is_some() {
                let _ = slint::quit_event_loop();
            }
        });
    }

    app.on_unknown_key(|t| {
        let cps: Vec<String> = t.chars().map(|c| format!("U+{:04X}", c as u32)).collect();
        eprintln!("UNKNOWN_KEY {}", cps.join(" "));
    });

    app.run().expect("event loop");
}
