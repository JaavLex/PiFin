//! The Wi-Fi screen: setup hotspot, connecting, and hotel login pages, driven by the
//! state file the Wi-Fi callback writes.

use crate::jellyfin::Auth;
use crate::remote::qr_image;
use crate::{profiles, settings, App};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::process::Child;
use std::rc::Rc;
use std::time::Duration;

/// Wi-Fi state published by the comitup callback (/run/pifin/wifi.json).
#[derive(Default, serde::Deserialize)]
#[serde(default)]
pub(crate) struct WifiState {
    pub(crate) state: String,
    pub(crate) ap_ssid: String,
    pub(crate) ap_pass: String,
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
        return (
            title,
            text,
            Some((w.portal_ssid.clone(), w.portal_pass.clone())),
        );
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

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Standard Wi-Fi QR payload; phones join the network when they scan it.
pub(crate) fn wifi_qr(ssid: &str, pass: &str) -> Option<slint::Image> {
    let esc = |v: &str| {
        v.chars()
            .flat_map(|c| match c {
                '\\' | ';' | ',' | ':' | '"' => vec!['\\', c],
                _ => vec![c],
            })
            .collect::<String>()
    };
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
            app.set_wifi_error(if w.failed.is_empty() {
                "".into()
            } else {
                format!(
                    "Couldn't connect to \"{}\". Check the password and try again.",
                    w.failed
                )
                .into()
            });
            if let Some((_, img)) = qr_cache {
                app.set_wifi_qr(img.clone());
                app.set_wifi_has_qr(true);
            }
            app.set_wifi_show(true);
            false
        }
        "CONNECTING" if age >= 8 || showing => {
            app.set_wifi_title(if w.ssid.is_empty() {
                "Connecting…".into()
            } else {
                format!("Connecting to \"{}\"…", w.ssid).into()
            });
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

/// Watches the Wi-Fi state file and the server address once per second. Keep the
/// timer alive.
pub(crate) fn watch(
    app: &App,
    load_home: Rc<dyn Fn()>,
    player: &Rc<RefCell<Option<Child>>>,
    auth: &Auth,
) -> slint::Timer {
    let wifi_timer = slint::Timer::default();
    let weak = app.as_weak();
    let player = player.clone();
    let auth = auth.clone();
    // the address at startup is the baseline: only a change after start restarts us
    let mut seen_url = settings::current().system.jellyfin_url.clone();
    let mut qr_cache: Option<(String, slint::Image)> = None;
    let wifi_file = settings::paths().wifi; // PIFIN_WIFI_FILE overrides, for tests
    wifi_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            let Some(app) = weak.upgrade() else { return };
            // the server address was changed (phone or TV): restart into the new one
            // (the daemon has already rewritten auth.json)
            let url = settings::current().system.jellyfin_url.clone();
            if server_changed(&mut seen_url, &url, &auth.server, player.borrow().is_none()) {
                eprintln!("SERVER address changed: restarting");
                profiles::restart(&["--user", &auth.user_id]);
            }
            let Ok(text) = std::fs::read_to_string(&wifi_file) else {
                return;
            };
            let Ok(w) = serde_json::from_str::<WifiState>(&text) else {
                return;
            };
            if apply_wifi(&app, &w, &mut qr_cache) {
                eprintln!("WIFI back: reloading home");
                load_home();
            }
        },
    );
    wifi_timer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_state_maps_to_the_login_screen() {
        // the JSON pifin-portal writes, comitup's fields kept
        let w: WifiState = serde_json::from_str(
            r#"{"state":"PORTAL","ssid":"Hotel","ap_ssid":"PiFin-setup","ap_pass":"x","time":1,
                "portal_ssid":"PiFin-login","portal_pass":"secret12","mac":"00:c0:ca:00:00:02",
                "hotspot":true,"expired":false}"#,
        )
        .unwrap();
        let (title, text, qr) = portal_screen(&w);
        assert_eq!(title, "This Wi-Fi needs a login");
        assert_eq!(
            qr,
            Some(("PiFin-login".to_string(), "secret12".to_string()))
        );
        assert!(
            text.contains("\"Hotel\"") && text.contains("PiFin carries on by itself"),
            "{text}"
        );
        assert!(!text.contains("00:c0:ca"), "{text}");
        // no adapter: no QR, the MAC to register at reception
        let w = WifiState {
            hotspot: false,
            portal_ssid: String::new(),
            portal_pass: String::new(),
            ..w
        };
        let (_, text, qr) = portal_screen(&w);
        assert_eq!(qr, None);
        assert!(
            text.contains("reception") && text.contains("00:c0:ca:00:00:02"),
            "{text}"
        );
        assert!(!text.contains("closed after 15 minutes"), "{text}");
        // the hotspot timed out: MAC screen, saying why
        let (_, text, qr) = portal_screen(&WifiState { expired: true, ..w });
        assert_eq!(qr, None);
        assert!(
            text.contains("closed after 15 minutes") && text.contains("00:c0:ca:00:00:02"),
            "{text}"
        );
    }

    #[test]
    fn server_change_restarts_once_per_new_url() {
        let mut seen = "http://a:8096".to_string();
        // same address as at startup (even with a mismatching auth.json): no restart
        assert!(!server_changed(
            &mut seen,
            "http://a:8096/",
            "http://old:8096",
            true
        ));
        // changed while a player runs: wait
        assert!(!server_changed(
            &mut seen,
            "http://b:8096",
            "http://a:8096",
            false
        ));
        // changed and idle: restart once
        assert!(server_changed(
            &mut seen,
            "http://b:8096",
            "http://a:8096",
            true
        ));
        // the daemon never rewrote auth.json: still the same url, no loop
        assert!(!server_changed(
            &mut seen,
            "http://b:8096",
            "http://a:8096",
            true
        ));
        // another new value fires again; one already applied does not
        assert!(server_changed(
            &mut seen,
            "http://c:8096",
            "http://a:8096",
            true
        ));
        assert!(!server_changed(
            &mut seen,
            "http://c:8096",
            "http://c:8096/",
            true
        ));
    }
}
