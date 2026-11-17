//! Browsable screens (home, show and season pages, search) and the focus logic.

use crate::images::{ImgJob, Slot, BACKDROP_GEN, BACKDROP_W, CARD_H, CARD_W};
use crate::jellyfin::Item;
use crate::remote::hide_remote_hint;
use crate::{App, Card, Row};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

/// One browsable screen: rows of items (home, or a series' seasons).
pub(crate) struct Screen {
    pub(crate) id: u64,
    pub(crate) titles: Vec<String>,
    pub(crate) items: Vec<Vec<Item>>,
    pub(crate) cards: Vec<Rc<VecModel<Card>>>,
    pub(crate) focus: Vec<i32>,
    pub(crate) cur_row: i32,
    pub(crate) search: Option<String>, // Some(query) for the search screen
}

/// UI-thread state.
pub(crate) struct State {
    pub(crate) screens: Vec<Screen>,
    pub(crate) rows: Rc<VecModel<Row>>,
    pub(crate) next_id: u64,
    pub(crate) backdrop_gen: u64,
    pub(crate) img_tx: mpsc::Sender<ImgJob>,
}

thread_local! {
    pub(crate) static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

pub(crate) fn with_state(f: impl FnOnce(&Rc<RefCell<State>>)) {
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
            ("resume", Some(u)) => {
                format!("▶  Resume from {}", fmt_ticks(u.playback_position_ticks))
            }
            ("resume", None) => "▶  Play".into(),
            _ => "↺  Play from start".into(),
        }
    } else if in_series || it.kind == "Person" || (it.kind == "Episode" && it.series_name.is_none())
    {
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
            if let Some(e) = it.index_number {
                v.push(format!("Episode {e}"));
            }
            if let Some(t) = it.run_time_ticks.filter(|t| *t >= 600_000_000) {
                v.push(format!("{} min", t / 600_000_000));
            }
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
pub(crate) fn poster_id(it: &Item, own: bool) -> String {
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

pub(crate) fn fmt_ticks(t: i64) -> String {
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
        parts.push(if m >= 60 {
            format!("{}h {:02}m", m / 60, m % 60)
        } else {
            format!("{m} min")
        });
    }
    if let Some(u) = &it.user_data {
        if u.playback_position_ticks > 0 {
            parts.push(format!(
                "resume at {}",
                fmt_ticks(u.playback_position_ticks)
            ));
        } else if u.played {
            parts.push("watched".into());
        }
    }
    parts.join("  ·  ")
}

/// Push (or, with `replace`, swap in place) a screen built from rows, queue its images.
pub(crate) fn install_screen(
    app: &App,
    st: &mut State,
    rows: Vec<(String, Vec<Item>)>,
    own_images: bool,
    replace: Option<usize>,
) {
    let id = st.next_id;
    st.next_id += 1;
    let mut screen = Screen {
        id,
        titles: vec![],
        items: vec![],
        cards: vec![],
        focus: vec![],
        cur_row: 0,
        search: None,
    };
    for (title, items) in rows {
        screen.cards.push(Rc::new(VecModel::from(
            items
                .iter()
                .map(|i| card_of(i, own_images))
                .collect::<Vec<_>>(),
        )));
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
                        slot: Slot::Card {
                            screen: id,
                            row: ri,
                            col: ci,
                        },
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
                if let Some(of) = old
                    .titles
                    .iter()
                    .position(|t| *t == screen.titles[r])
                    .and_then(|o| old.focus.get(o))
                {
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
pub(crate) fn show_top(app: &App, st: &mut State) {
    let Some(sc) = st.screens.last() else { return };
    let rows: Vec<Row> = sc
        .titles
        .iter()
        .zip(&sc.cards)
        .zip(&sc.focus)
        .map(|((t, c), f)| Row {
            title: t.clone().into(),
            cards: ModelRc::from(c.clone()),
            focus: *f,
        })
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
    let (r, c) = (
        sc.cur_row,
        sc.focus.get(sc.cur_row as usize).copied().unwrap_or(0),
    );
    app.set_cur_row(r);
    let app_weak = app.as_weak();
    // run the focus update after this borrow ends
    slint::Timer::single_shot(Duration::from_millis(0), move || {
        if let Some(a) = app_weak.upgrade() {
            a.invoke_moved(r, c);
        }
    });
}

/// Focus moves: clamp, remember the column, update the header and the backdrop.
pub(crate) fn install_focus(app: &App, state: &Rc<RefCell<State>>) {
    let backdrop_timer = Rc::new(slint::Timer::default());
    let weak = app.as_weak();
    let state = state.clone();
    app.on_moved(move |r, c| {
        let Some(app) = weak.upgrade() else { return };
        if app.get_remote_show() && app.get_cur_row() != r {
            hide_remote_hint(&app);
        }
        let mut st = state.borrow_mut();
        let st = &mut *st;
        let Some(sc) = st.screens.last_mut() else {
            return;
        };
        if sc.items.is_empty() {
            return;
        }
        let r = r.clamp(0, sc.items.len() as i32 - 1) as usize;
        let col = if c < 0 {
            sc.focus[r]
        } else {
            c.clamp(0, sc.items[r].len() as i32 - 1)
        };
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
        backdrop_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(250),
            move || {
                let _ = tx.send(ImgJob {
                    slot: Slot::Backdrop { gen },
                    item_id: id.clone(),
                    kind: "Backdrop",
                    w: BACKDROP_W,
                    h: 720,
                });
            },
        );
    });
}
