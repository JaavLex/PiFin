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
    let path = std::env::var("PIFIN_AUTH").unwrap_or_else(|_| {
        format!("{}/.config/pifin/auth.json", std::env::var("HOME").unwrap_or_default())
    });
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

/// What an image result is for.
#[derive(Clone, Copy)]
enum Slot {
    Card { row: usize, col: usize },
    Backdrop { gen: u64 },
}

struct ImgJob {
    slot: Slot,
    item_id: String,
    kind: &'static str,
    w: u32,
    h: u32,
}

/// UI-thread state.
struct State {
    items: Vec<Vec<Item>>,
    cards: Vec<Rc<VecModel<Card>>>,
    rows: Rc<VecModel<Row>>,
    backdrop_gen: u64,
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

fn card_of(it: &Item) -> Card {
    let progress = match (&it.user_data, it.run_time_ticks) {
        (Some(u), Some(run)) if run > 0 && u.playback_position_ticks > 0 => {
            (u.playback_position_ticks as f64 / run as f64).clamp(0.0, 1.0) as f32
        }
        _ => 0.0,
    };
    Card {
        id: it.id.clone().into(),
        title: it.title().into(),
        subtitle: it.subtitle().into(),
        image: Default::default(),
        has_image: false,
        progress,
    }
}

/// Poster for a card: the item's own, else the series poster (episodes).
fn poster_id(it: &Item) -> String {
    if it.kind != "Episode" && it.image_tags.contains_key("Primary") {
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
    if let Some(t) = it.run_time_ticks {
        let m = t / 600_000_000;
        parts.push(if m >= 60 { format!("{}h {:02}m", m / 60, m % 60) } else { format!("{m} min") });
    }
    if let Some(u) = &it.user_data {
        if u.playback_position_ticks > 0 {
            parts.push(format!("resume at {}", fmt_ticks(u.playback_position_ticks)));
        }
    }
    parts.join("  ·  ")
}

fn main() {
    let t0 = Instant::now();
    let app = App::new().expect("create window");
    let auth = match load_auth() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("auth: {e}");
            app.set_heading("Not signed in".into());
            app.set_info(e.into());
            app.run().ok();
            return;
        }
    };
    let client = Arc::new(Client::new(auth.clone()));

    let state = Rc::new(RefCell::new(State {
        items: vec![],
        cards: vec![],
        rows: Rc::new(VecModel::default()),
        backdrop_gen: 0,
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
    let (img_tx, img_rx) = mpsc::channel::<ImgJob>();
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
            let bytes = match client.image(&job.item_id, job.kind, job.w) {
                Ok(b) => b,
                Err(_) => continue, // no image of that kind: the card keeps its title
            };
            let Some(buf) = decode(&bytes, job.slot, job.w, job.h) else { continue };
            let slot = job.slot;
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                let img = slint::Image::from_rgba8(buf);
                with_state(|state| {
                    let st = state.borrow();
                    match slot {
                        Slot::Card { row, col } => {
                            if let Some(m) = st.cards.get(row) {
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

    // ---- home rows (API thread) ---------------------------------------------
    {
        let client = client.clone();
        let weak = app.as_weak();
        let img_tx = img_tx.clone();
        app.set_status("Loading…".into());
        std::thread::spawn(move || {
            let mut rows: Vec<(String, Vec<Item>)> = vec![];
            let mut push = |title: String, r: Result<Vec<Item>, String>| match r {
                Ok(v) if !v.is_empty() => rows.push((title, v)),
                Ok(_) => {}
                Err(e) => eprintln!("{e}"),
            };
            push("Continue Watching".into(), client.resume());
            push("Next Up".into(), client.next_up());
            if let Ok(views) = client.views() {
                for v in views.iter().filter(|v| matches!(v.collection_type.as_deref(), Some("movies" | "tvshows"))) {
                    push(format!("Latest {}", v.name), client.latest(&v.id));
                }
            }
            eprintln!("HOME_DATA_MS={} rows={}", t0.elapsed().as_millis(), rows.len());
            // posters: first 8 of every row first (what is on screen), then the rest
            for pass in 0..2 {
                for (ri, (_, items)) in rows.iter().enumerate() {
                    for (ci, it) in items.iter().enumerate() {
                        if (pass == 0) == (ci < 8) {
                            let _ = img_tx.send(ImgJob {
                                slot: Slot::Card { row: ri, col: ci },
                                item_id: poster_id(it),
                                kind: "Primary",
                                w: CARD_W,
                                h: CARD_H,
                            });
                        }
                    }
                }
            }
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                with_state(|state| {
                    let mut st = state.borrow_mut();
                    for (title, items) in rows {
                        let cards = Rc::new(VecModel::from(items.iter().map(card_of).collect::<Vec<_>>()));
                        st.rows.push(Row { title: title.into(), cards: ModelRc::from(cards.clone()), focus: 0 });
                        st.cards.push(cards);
                        st.items.push(items);
                    }
                });
                app.set_status("".into());
                app.invoke_moved(0, 0);
                eprintln!("HOME_SHOWN_MS={}", t0.elapsed().as_millis());
            });
        });
    }

    // ---- focus moves: clamp, remember column, update header + backdrop ---------
    let backdrop_timer = Rc::new(slint::Timer::default());
    {
        let weak = app.as_weak();
        let state = state.clone();
        let img_tx = img_tx.clone();
        let backdrop_timer = backdrop_timer.clone();
        app.on_moved(move |r, c| {
            let Some(app) = weak.upgrade() else { return };
            let mut st = state.borrow_mut();
            if st.items.is_empty() {
                return;
            }
            let r = r.clamp(0, st.items.len() as i32 - 1) as usize;
            let mut row = st.rows.row_data(r).unwrap();
            let col = if c < 0 { row.focus } else { c.clamp(0, st.items[r].len() as i32 - 1) } as usize;
            if row.focus != col as i32 {
                row.focus = col as i32;
                st.rows.set_row_data(r, row);
            }
            app.set_cur_row(r as i32);
            let it = st.items[r][col].clone();
            let it = &it;
            app.set_heading(it.title().into());
            app.set_info(info_line(it).into());
            app.set_overview(it.overview.clone().unwrap_or_default().into());
            // backdrop once the focus settles for 250 ms (no fetch storm while scrolling)
            st.backdrop_gen += 1;
            let gen = st.backdrop_gen;
            let id = backdrop_id(it);
            let tx = img_tx.clone();
            backdrop_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(250), move || {
                let _ = tx.send(ImgJob { slot: Slot::Backdrop { gen }, item_id: id.clone(), kind: "Backdrop", w: BACKDROP_W, h: 720 });
            });
        });
    }

    // ---- play: hand off to mpv, poll for its exit at 2 Hz --------------------
    let player: Rc<RefCell<Option<Child>>> = Rc::new(RefCell::new(None));
    let poll = Rc::new(slint::Timer::default());
    {
        let weak = app.as_weak();
        let state = state.clone();
        let player = player.clone();
        let poll = poll.clone();
        let auth = auth.clone();
        app.on_activate(move |r, c| {
            if player.borrow().is_some() {
                return;
            }
            let st = state.borrow();
            let Some(it) = st.items.get(r as usize).and_then(|v| v.get(c as usize)) else { return };
            if !matches!(it.kind.as_str(), "Movie" | "Episode") {
                if let Some(a) = weak.upgrade() {
                    a.set_status("Series pages come next".into());
                }
                return;
            }
            let url = format!(
                "{}/Videos/{}/stream?static=true&mediaSourceId={}&api_key={}",
                auth.server.trim_end_matches('/'), it.id, it.id, auth.token
            );
            let start = it.user_data.as_ref().map(|u| u.playback_position_ticks as f64 / 1e7).unwrap_or(0.0);
            let title = match (&it.series_name, it.parent_index_number, it.index_number) {
                (Some(s), Some(se), Some(ep)) => format!("{s} - S{se}E{ep} - {}", it.name),
                _ => it.name.clone(),
            };
            match mpv_command(&url, start, &title).spawn() {
                Ok(child) => {
                    eprintln!("MPV_START id={} t={}ms", it.id, t0.elapsed().as_millis());
                    *player.borrow_mut() = Some(child);
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
                            eprintln!("MPV_EXIT t={}ms", t0.elapsed().as_millis());
                            if let Some(a) = weak.upgrade() {
                                a.window().request_redraw();
                            }
                        }
                    });
                }
                Err(e) => eprintln!("mpv spawn failed: {e}"),
            }
        });
    }
    app.on_back(move || {
        if std::env::var_os("PIFIN_EXIT").is_some() {
            let _ = slint::quit_event_loop();
        }
    });

    app.run().expect("event loop");
}
