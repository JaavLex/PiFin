//! Search: on-screen keyboard, results 0.4 s after the last key.

use crate::jellyfin::Client;
use crate::remote::hide_remote_hint;
use crate::screens::{install_screen, show_top, with_state, State};
use crate::App;
use slint::{ComponentHandle, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// Sets up the keyboard and returns `open_search(query)`.
pub(crate) fn install(
    app: &App,
    state: &Rc<RefCell<State>>,
    client: &Arc<Client>,
) -> Rc<dyn Fn(String)> {
    let kb: Vec<slint::SharedString> = ('a'..='z')
        .chain('0'..='9')
        .map(|c| c.to_string().into())
        .chain(["space", "delete", "clear"].iter().map(|k| (*k).into()))
        .collect();
    app.set_kb_keys(ModelRc::new(VecModel::from(kb)));
    let search_timer = Rc::new(slint::Timer::default());
    let run_search: Rc<dyn Fn(String)> = {
        let weak = app.as_weak();
        let client = client.clone();
        let search_timer = search_timer.clone();
        Rc::new(move |q: String| {
            let weak = weak.clone();
            let client = client.clone();
            search_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(400),
                move || {
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
                                if st.screens.last().and_then(|s| s.search.as_deref())
                                    != Some(q.as_str())
                                {
                                    return;
                                }
                                let rows = if items.is_empty() {
                                    vec![]
                                } else {
                                    vec![(format!("Results for \"{q}\""), items)]
                                };
                                install_screen(&app, &mut st, rows, false, Some(top));
                            });
                        });
                    });
                },
            );
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
            let is_search = st
                .screens
                .last()
                .map(|s| s.search.is_some())
                .unwrap_or(false);
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
            let Some(sc) = st.screens.last_mut() else {
                return;
            };
            let Some(q) = sc.search.as_mut() else { return };
            match key.as_str() {
                "space" => q.push(' '),
                "delete" => {
                    q.pop();
                }
                "clear" => q.clear(),
                k => q.push_str(k),
            }
            let q = q.clone();
            app.set_query(q.clone().into());
            drop(st);
            run_search(q);
        });
    }
    open_search
}
