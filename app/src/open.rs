//! Opening an item: movie, show and season pages, or playback.

use crate::home::{fetch_movie, fetch_season, fetch_seasons};
use crate::jellyfin::Item;
use crate::playback::{link_bps, record_link};
use crate::remote::{self, hide_remote_hint};
use crate::screens::{install_screen, with_state};
use crate::{player, App, Ctx};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// Returns `open_item(item, play)`: `play` = true plays episodes directly; false opens
/// an episode's season page. Also handles OK on a card and Back while starting.
pub(crate) fn install(app: &App, ctx: &Ctx) -> Rc<dyn Fn(Item, bool)> {
    let poll = Rc::new(slint::Timer::default());
    let start_gen = Rc::new(Cell::new(0u64)); // one per Play, for the "slow" hint
    let open_item: Rc<dyn Fn(Item, bool)> = {
        let ctx = ctx.clone();
        Rc::new(move |it: Item, play: bool| {
            let (weak, player, client) = (&ctx.app, &ctx.player, &ctx.client);
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
                            with_state(|state| {
                                install_screen(&app, &mut state.borrow_mut(), rows, false, None)
                            });
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
                "Episode" if !play => it.season_id.clone().map(|sid| {
                    (
                        Item {
                            id: sid,
                            kind: "Season".into(),
                            name: it.season_name.clone().unwrap_or_else(|| "Season".into()),
                            series_id: it.series_id.clone(),
                            series_name: it.series_name.clone(),
                            ..Default::default()
                        },
                        true,
                    )
                }),
                _ => None,
            };
            if let Some((target, is_season)) = page {
                let client = client.clone();
                let weak = weak.clone();
                if let Some(a) = weak.upgrade() {
                    a.set_status("Loading…".into());
                }
                std::thread::spawn(move || {
                    let rows = if is_season {
                        fetch_season(&client, &target)
                    } else {
                        fetch_seasons(&client, &target.id)
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        app.set_status(if rows.is_empty() {
                            "Nothing here".into()
                        } else {
                            "".into()
                        });
                        if !rows.is_empty() {
                            // episode lists use the episodes' own stills and names
                            with_state(|state| {
                                install_screen(&app, &mut state.borrow_mut(), rows, is_season, None)
                            });
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
                        if let Some(a) = weak
                            .upgrade()
                            .filter(|a| a.get_starting() && start_gen.get() == gen)
                        {
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
                    let full = if it.media_sources.is_empty() {
                        client.item(&it.id)
                    } else {
                        Ok(it)
                    };
                    let full = full.map(|mut f| {
                        if let Some(d) = defaults {
                            // no answer: the old behaviour (mpv's / the file's own choice)
                            let d = d.join().ok().and_then(|r| {
                                r.map_err(|e| eprintln!("PLAYBACK_DEFAULTS: {e}")).ok()
                            });
                            // (-1, -1) matches no stream, so `tracks` leaves everything to mpv
                            f.defaults = Some(d.unwrap_or((Some(-1), Some(-1))));
                        }
                        f
                    });
                    if let Some(p) = probe {
                        let rate = p.join().ok().flatten().unwrap_or(0);
                        eprintln!("LINK_MBPS={:.1}", rate as f64 / 1e6);
                        record_link(rate);
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
                        remote::dispatch(&app, "play", Some(full));
                    });
                });
                return;
            }
            player::start(&ctx, &poll, it);
        })
    };
    player::install_cancel(app, &ctx.player);
    {
        let state = ctx.state.clone();
        let open_item = open_item.clone();
        app.on_activate(move |r, c| {
            let it = {
                let st = state.borrow();
                let Some(it) = st
                    .screens
                    .last()
                    .and_then(|s| s.items.get(r as usize))
                    .and_then(|v| v.get(c as usize))
                else {
                    return;
                };
                it.clone()
            };
            // movies open their page; episodes and the page's Play cards play
            let play = it.kind != "Movie" || it.action.is_some();
            open_item(it, play);
        });
    }
    open_item
}
