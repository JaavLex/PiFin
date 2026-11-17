//! PiFin: lightweight Jellyfin browser for the Raspberry Pi.
//!
//! Runs under cage as a Wayland client. Playback is handed off to the native
//! mpv process (hardware decoding + pifin-osd.lua overlay); the UI does no work
//! while mpv plays and comes back on the same screen when it exits.
//!
//! Threads: UI (Slint, owns all models), one API thread, two image workers.
//! Workers hand results back with `invoke_from_event_loop`; a generation counter
//! drops backdrop results that arrive after the focus moved on.

mod frames;
mod home;
mod images;
mod jellyfin;
mod modeswitch;
mod open;
mod playback;
mod player;
mod profile_screen;
mod profiles;
mod remote;
mod screens;
mod search;
mod settings;
mod settings_ui;
mod setup;
mod wifi_screen;

use images::ImgJob;
use jellyfin::{Auth, Client};
use profile_screen::{initial, show_profiles};
use remote::hide_remote_hint;
use screens::State;
use slint::{ModelRc, SharedPixelBuffer, VecModel};
use std::cell::RefCell;
use std::process::Child;
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::Instant;

slint::include_modules!();

/// What the item and playback handlers share.
#[derive(Clone)]
struct Ctx {
    app: slint::Weak<App>,
    state: Rc<RefCell<State>>,
    client: Arc<Client>,
    auth: Auth,
    /// the running mpv, if any
    player: Rc<RefCell<Option<Child>>>,
    /// process start, for the timing log lines
    t0: Instant,
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
        app.set_logo(slint::Image::from_rgba8(
            SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()),
        ));
    }
    // which user: --user <id> (after a pick), the only one, or ask
    let args: Vec<String> = std::env::args().collect();
    let store = profiles::Store::load();
    let Some(store) = store.filter(|_| !args.iter().any(|a| a == "--add-user")) else {
        let adding = profiles::Store::load().is_some();
        eprintln!(
            "{}; starting Quick Connect sign-in",
            if adding {
                "adding a user"
            } else {
                "no saved login"
            }
        );
        let _setup = setup::sign_in(&app, adding);
        app.run().ok();
        return;
    };
    let picked = args
        .iter()
        .position(|a| a == "--user")
        .and_then(|i| args.get(i + 1))
        .cloned();
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
    app.set_user_index(
        store
            .users
            .iter()
            .position(|p| p.user_id == profile.user_id)
            .unwrap_or(0) as i32,
    );
    profile_screen::load_avatar(&app, &auth);
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
    screens::STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    let _frame_timer = frames::frame_log(&app, t0);
    images::spawn_workers(&app, &client, img_rx);
    let load_home = home::loader(&app, &client, t0);
    load_home();
    let player: Rc<RefCell<Option<Child>>> = Rc::new(RefCell::new(None));
    let _wifi_timer = wifi_screen::watch(&app, load_home, &player, &auth);
    screens::install_focus(&app, &state);
    let ctx = Ctx {
        app: app.as_weak(),
        state: state.clone(),
        client: client.clone(),
        auth: auth.clone(),
        player,
        t0,
    };
    let open_item = open::install(&app, &ctx);
    let open_search = search::install(&app, &state, &client);
    remote::listen(&app, &client, &state, &open_item, &open_search);
    remote::boot_hint(&app);

    // Back: pop a screen; at home it opens "Who's watching?" (PIFIN_EXIT quits, for probes)
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
                screens::show_top(&app, &mut st);
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
        settings_ui::install(
            &app,
            Arc::new(move || {
                let rate = probe_client.server_test();
                if let Some(r) = rate {
                    playback::record_link(r);
                }
                rate
            }),
            client.clone(),
            profile.name.clone(),
        );
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
