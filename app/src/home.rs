//! Rows from the server: the home screen (with its disk cache) and the movie,
//! show and season pages.

use crate::images::cache_dir;
use crate::jellyfin::{Client, Item};
use crate::screens::{install_screen, with_state};
use crate::App;
use slint::ComponentHandle;
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Rows for the home screen.
type Rows = Vec<(String, Vec<Item>)>;

/// Home rows, all requests in parallel. A cold server can take over a minute for
/// one query (Continue Watching took 81 s once) while the others answer in
/// 50 ms, so `publish(rows)` gets what is there after `PARTIAL_AFTER`, then the
/// full set when the last request finishes. Returns false if any request failed.
pub(crate) fn fetch_home(
    client: &Arc<Client>,
    partial: bool,
    mut publish: impl FnMut(Rows),
) -> bool {
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
    let mut slots: Vec<(String, Option<Res>)> =
        vec![("Continue Watching".into(), None), ("Next Up".into(), None)];
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
        slots
            .iter()
            .filter_map(|(t, r)| match r {
                Some(Ok(v)) if !v.is_empty() => Some((t.clone(), v.clone())),
                _ => None,
            })
            .collect()
    };
    while pending > 0 {
        let left = PARTIAL_AFTER.saturating_sub(t0.elapsed());
        let wait = if !published && !left.is_zero() {
            left
        } else {
            Duration::from_secs(120)
        };
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
                    let (id, movies) =
                        (v.id.clone(), v.collection_type.as_deref() == Some("movies"));
                    spawn(
                        slot,
                        Box::new(move |c| if movies { c.latest(&id) } else { c.shows(&id) }),
                    );
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
                eprintln!(
                    "HOME_PARTIAL_MS={} rows={} pending={pending}",
                    t0.elapsed().as_millis(),
                    rows.len()
                );
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
pub(crate) fn fetch_seasons(client: &Client, series_id: &str) -> Vec<(String, Vec<Item>)> {
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
pub(crate) fn fetch_season(client: &Client, season: &Item) -> Vec<(String, Vec<Item>)> {
    let series_id = season.series_id.clone().unwrap_or_default();
    match client.episodes(&series_id, &season.id) {
        Ok(v) if !v.is_empty() => vec![(
            format!(
                "{} · {}",
                season.series_name.clone().unwrap_or_default(),
                season.name
            ),
            v,
        )],
        Ok(_) => vec![],
        Err(e) => {
            eprintln!("{e}");
            vec![]
        }
    }
}

/// Rows for a movie page: Play/Resume actions, then the cast.
pub(crate) fn fetch_movie(client: &Client, id: &str) -> Vec<(String, Vec<Item>)> {
    let Ok(movie) = client.item(id) else {
        return vec![];
    };
    let resume = movie
        .user_data
        .as_ref()
        .map(|u| u.playback_position_ticks > 0)
        .unwrap_or(false);
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
    let cast: Vec<Item> = movie
        .people
        .iter()
        .filter(|p| p.kind == "Actor" || p.kind == "Director")
        .take(20)
        .map(|p| Item {
            id: p.id.clone(),
            name: p.name.clone(),
            kind: "Person".into(),
            overview: p
                .role
                .clone()
                .filter(|r| !r.is_empty())
                .or_else(|| (p.kind == "Director").then(|| "Director".into())),
            image_tags: p
                .primary_image_tag
                .iter()
                .map(|t| ("Primary".to_string(), t.clone()))
                .collect(),
            ..Default::default()
        })
        .collect();
    if !cast.is_empty() {
        rows.push(("Cast & crew".to_string(), cast));
    }
    rows
}

/// The last full home screen of a user, shown at once at the next start while
/// the server is asked again (a cold server took 87 s once; the network itself
/// comes up ~7 s after the app at boot).
fn home_cache(user_id: &str) -> std::path::PathBuf {
    cache_dir().join(format!("home-{user_id}.json"))
}

pub(crate) fn save_home(user_id: &str, rows: &Rows) {
    let p = home_cache(user_id);
    let _ = std::fs::create_dir_all(cache_dir());
    let tmp = p.with_extension("tmp");
    if serde_json::to_vec(rows)
        .ok()
        .is_some_and(|b| std::fs::write(&tmp, b).is_ok())
    {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn load_home_cache(user_id: &str) -> Option<Rows> {
    serde_json::from_slice(&std::fs::read(home_cache(user_id)).ok()?)
        .ok()
        .filter(|r: &Rows| !r.is_empty())
}

/// Loads the home screen on the API thread; retries while the server is unreachable.
/// Calls while a load runs are ignored.
pub(crate) fn loader(app: &App, client: &Arc<Client>, t0: Instant) -> Rc<dyn Fn()> {
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
                if let (Some(rows), Some(app)) =
                    (load_home_cache(&client.auth.user_id), weak.upgrade())
                {
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
                    eprintln!(
                        "HOME_DATA_MS={} rows={}",
                        t0.elapsed().as_millis(),
                        rows.len()
                    );
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
}
