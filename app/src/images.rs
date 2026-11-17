//! Posters and backdrops: decoded and prepared off the UI thread by two workers,
//! with a disk cache of the server-resized images.

use crate::jellyfin::Client;
use crate::screens::with_state;
use crate::App;
use slint::{ComponentHandle, Model, Rgba8Pixel, SharedPixelBuffer};
use std::sync::{mpsc, Arc, Mutex};

/// Poster size in physical px (150x225 logical at scale 1.5).
pub(crate) const CARD_W: u32 = 225;

pub(crate) const CARD_H: u32 = 338;

pub(crate) const BACKDROP_W: u32 = 1280;

/// Background colour of the UI (#0b0b12), used to bake the backdrop fade.
const BG: [f32; 3] = [11.0, 11.0, 18.0];

/// Decode an image off the UI thread and prepare it so the GPU only has to draw
/// a plain opaque/textured quad: no clipping, no opacity, no gradient layers.
fn decode(bytes: &[u8], slot: Slot, w: u32, h: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut rgba = img
        .resize_to_fill(w, h, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let (iw, ih) = rgba.dimensions();
    match slot {
        // rounded corners baked into the alpha channel
        Slot::Card { .. } => {
            let r = 14.0f32;
            for (x, y, px) in rgba.enumerate_pixels_mut() {
                let cx = if (x as f32) < r {
                    r - x as f32 - 0.5
                } else if x as f32 >= iw as f32 - r {
                    x as f32 + 0.5 - (iw as f32 - r)
                } else {
                    0.0
                };
                let cy = if (y as f32) < r {
                    r - y as f32 - 0.5
                } else if y as f32 >= ih as f32 - r {
                    y as f32 + 0.5 - (ih as f32 - r)
                } else {
                    0.0
                };
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
                for (c, bg) in BG.iter().enumerate() {
                    px.0[c] = (px.0[c] as f32 * a + bg * (1.0 - a)) as u8;
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
pub(crate) enum Slot {
    Card { screen: u64, row: usize, col: usize },
    Backdrop { gen: u64 },
}

/// ~/.cache/pifin (or $XDG_CACHE_HOME/pifin).
pub(crate) fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    std::path::PathBuf::from(base).join("pifin")
}

/// Latest backdrop request: workers skip older ones still in the queue.
pub(crate) static BACKDROP_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Server-resized images kept on disk (~25 KB a poster, ~300 KB a backdrop):
/// the home screen fills from the SD card at boot, and a slow or remote server
/// (a hotel, a friend's place) is asked again only after a week.
pub(crate) mod img_cache {
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
        let Ok(rd) = std::fs::read_dir(dir()) else {
            return;
        };
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

pub(crate) struct ImgJob {
    pub(crate) slot: Slot,
    pub(crate) item_id: String,
    pub(crate) kind: &'static str,
    pub(crate) w: u32,
    pub(crate) h: u32,
}

/// Two workers that fetch (or read from the cache), decode and hand images to the UI.
pub(crate) fn spawn_workers(app: &App, client: &Arc<Client>, img_rx: mpsc::Receiver<ImgJob>) {
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
            let Some(bytes) = img_cache::get(&key, || client.image(&job.item_id, job.kind, job.w))
            else {
                continue;
            };
            let Some(buf) = decode(&bytes, job.slot, job.w, job.h) else {
                continue;
            };
            let slot = job.slot;
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                let img = slint::Image::from_rgba8(buf);
                with_state(|state| {
                    let st = state.borrow();
                    match slot {
                        Slot::Card { screen, row, col } => {
                            let Some(sc) = st.screens.iter().find(|s| s.id == screen) else {
                                return;
                            };
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
}
