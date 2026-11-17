//! "Who's watching?" and the signed-in user in the top bar.

use crate::images::img_cache;
use crate::jellyfin::{Auth, Client};
use crate::{profiles, App, Card};
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::rc::Rc;

/// Avatar as a circle (alpha baked in, so the GPU draws a plain quad).
fn avatar(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    const S: u32 = 240;
    let img = image::load_from_memory(bytes).ok()?;
    let mut rgba = img
        .resize_to_fill(S, S, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let r = S as f32 / 2.0;
    for (x, y, px) in rgba.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - r, y as f32 + 0.5 - r);
        let a = (r - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
        px.0[3] = (px.0[3] as f32 * a) as u8;
    }
    Some(SharedPixelBuffer::clone_from_slice(rgba.as_raw(), S, S))
}

pub(crate) fn initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
}

/// "Who's watching?". `current`: the user this process runs as (None at boot,
/// where Back does nothing). Picking another user restarts as that user.
pub(crate) fn show_profiles(app: &App, store: &profiles::Store, current: Option<String>) {
    let cards: Vec<Card> = store
        .users
        .iter()
        .map(|p| Card {
            id: p.user_id.clone().into(),
            title: p.name.clone().into(),
            subtitle: initial(&p.name).into(),
            ..Default::default()
        })
        .chain(std::iter::once(Card {
            id: "add".into(),
            title: "Add user".into(),
            ..Default::default()
        }))
        .collect();
    let focus = store
        .users
        .iter()
        .position(|p| p.user_id == store.user_id)
        .unwrap_or(0);
    let model = Rc::new(VecModel::from(cards));
    app.set_profiles(ModelRc::from(model.clone()));
    app.set_profile_focus(focus as i32);
    app.set_profiles_show(true);

    let weak = app.as_weak();
    {
        let current = current.clone();
        app.on_profile_pick(move |i| {
            let Some(app) = weak.upgrade() else { return };
            let Some(card) = app.get_profiles().row_data(i as usize) else {
                return;
            };
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
            let Ok((name, bytes)) = Client::new(auth.clone()).me(240) else {
                continue;
            };
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
                if let Some(p) = st
                    .users
                    .iter_mut()
                    .find(|p| p.user_id == id && p.name != name)
                {
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

/// The top bar's avatar: from the disk cache, refreshed from the server.
pub(crate) fn load_avatar(app: &App, auth: &Auth) {
    let weak = app.as_weak();
    let auth = auth.clone();
    std::thread::spawn(move || {
        let key = format!("avatar-{}", auth.user_id);
        let bytes = img_cache::get(&key, || match Client::new(auth.clone()).me(96) {
            Ok((_, Some(b))) => Ok(b),
            Ok((_, None)) => Err("no avatar".into()),
            Err(e) => Err(e),
        });
        let Some(buf) = bytes.as_deref().and_then(avatar) else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                app.set_user_image(slint::Image::from_rgba8(buf));
                app.set_user_has_image(true);
            }
        });
    });
}
