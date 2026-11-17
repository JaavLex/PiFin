//! First setup on a fresh image, and the Quick Connect sign-in.

use crate::remote::{local_ip, qr_image, remote_url};
use crate::wifi_screen::{wifi_qr, WifiState};
use crate::{jellyfin, profiles, settings, App};
use slint::ComponentHandle;
use std::time::Duration;

/// JELLYFIN_URL from /etc/pifin/app.conf (world-readable, written by build.py).
fn server_url() -> Option<String> {
    let text = std::fs::read_to_string("/etc/pifin/app.conf").ok()?;
    text.lines().find_map(|l| {
        let v = l
            .strip_prefix("JELLYFIN_URL=")?
            .trim()
            .trim_matches(|c| c == '\'' || c == '"');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// What the first-setup screen shows (fresh image: no Jellyfin server address yet).
#[derive(Debug, PartialEq)]
enum SetupStep {
    /// the phone set the server: on to the Quick Connect sign-in
    Done,
    /// no Wi-Fi known: the setup hotspot's QR code (join it, pick a network)
    Wifi,
    /// on a network: the phone remote's QR code, which opens the phone's setup wizard
    Phone,
    NoNetwork,
}

fn setup_step(server: &str, wifi_state: &str, ip: Option<&str>) -> SetupStep {
    if !server.is_empty() {
        SetupStep::Done
    } else if wifi_state == "HOTSPOT" {
        SetupStep::Wifi
    } else if ip.is_some() {
        SetupStep::Phone
    } else {
        SetupStep::NoNetwork
    }
}

/// First setup on the TV: a QR code for the phone, which does the typing (server address,
/// Tailscale key, hotspot password, PIN). Checked every 2 s; once the server address is
/// there the app restarts into the Quick Connect sign-in. Keep the timer alive.
fn setup_screen(app: &App) -> slint::Timer {
    let weak = app.as_weak();
    let mut cache: Option<(String, slint::Image)> = None;
    let mut tick = move || {
        let Some(app) = weak.upgrade() else { return };
        let server = settings::current().system.jellyfin_url.clone();
        let w: WifiState = std::fs::read_to_string(settings::paths().wifi)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let ip = local_ip().map(|i| i.to_string());
        app.set_wifi_pickable(false);
        app.set_wifi_error("".into());
        let qr_text = match setup_step(&server, &w.state, ip.as_deref()) {
            SetupStep::Done => {
                eprintln!("SETUP done: restarting into the sign-in");
                profiles::restart(&[]) // never returns
            }
            SetupStep::Wifi => {
                app.set_wifi_title("Set up PiFin: Wi-Fi".into());
                app.set_wifi_text(format!(
                    "1.  Scan the code with your phone to join the network \"{}\" (password: {}).\n\n\
                     2.  A page opens: pick your Wi-Fi and enter its password. If it doesn't open, go to http://10.41.0.1\n\n\
                     3.  PiFin joins it, then shows the next code here.",
                    w.ap_ssid, w.ap_pass
                ).into());
                Some(format!("WIFI\n{}\n{}", w.ap_ssid, w.ap_pass))
            }
            SetupStep::Phone => {
                app.set_wifi_title("Set up PiFin".into());
                app.set_wifi_text(
                    "1.  Scan the code with your phone (on the same Wi-Fi as PiFin).\n\n\
                     2.  Enter your Jellyfin server address on the phone, and optionally a Tailscale key, \
                     the setup hotspot password and a PIN.\n\n\
                     3.  Then sign in to Jellyfin here, with Quick Connect."
                        .into(),
                );
                let url = remote_url(
                    ip.as_deref().unwrap_or(""),
                    settings::pair_token().as_deref(),
                );
                Some(format!("URL\n{url}"))
            }
            SetupStep::NoNetwork => {
                app.set_wifi_title("Set up PiFin".into());
                app.set_wifi_text("Waiting for a network…".into());
                None
            }
        };
        match qr_text {
            Some(key) => {
                if cache.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
                    let img = match key.split_once('\n') {
                        Some(("WIFI", _)) => wifi_qr(&w.ap_ssid, &w.ap_pass),
                        Some((_, url)) => qr_image(url),
                        None => None,
                    };
                    cache = img.map(|i| (key, i));
                }
                if let Some((_, img)) = &cache {
                    app.set_wifi_qr(img.clone());
                }
                app.set_wifi_has_qr(cache.is_some());
            }
            None => app.set_wifi_has_qr(false),
        }
        app.set_wifi_show(true);
    };
    tick();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(2), tick);
    timer
}

/// Sign a user in with Quick Connect: show the code full screen, wait for
/// approval, add the user to auth.json and restart as that user. `adding`:
/// other users exist, so Back returns to "Who's watching?". No server address yet
/// (fresh image): the first-setup screen instead, whose timer the caller keeps.
pub(crate) fn sign_in(app: &App, adding: bool) -> Option<slint::Timer> {
    app.set_wifi_title("Sign in to Jellyfin".into());
    app.set_wifi_has_qr(false);
    app.set_wifi_show(true);
    if adding {
        app.on_cancel(|| profiles::restart(&[]));
    }
    // a new user joins the server the others are on; app.conf is for the first sign-in
    let from_settings =
        || Some(settings::current().system.jellyfin_url.clone()).filter(|s| !s.is_empty());
    let Some(server) = profiles::Store::load()
        .map(|s| s.server)
        .filter(|s| !s.is_empty())
        .or_else(server_url)
        .or_else(from_settings)
    else {
        return Some(setup_screen(app));
    };
    app.set_wifi_text(format!("Connecting to {server}…").into());
    // one device id per user (Jellyfin ties a token to a device)
    let device_id = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .unwrap_or_default()
        .trim()
        .replace('-', "");
    let back = if adding { "\n\nBack to cancel." } else { "" };
    let weak = app.as_weak();
    std::thread::spawn(move || loop {
        let set = |title: String, text: String, err: String| {
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(a) = w.upgrade() {
                    if !title.is_empty() {
                        a.set_wifi_title(title.into());
                    }
                    a.set_wifi_text(text.into());
                    a.set_wifi_error(err.into());
                }
            });
        };
        let qc = match jellyfin::QuickConnect::start(&server, &device_id) {
            Ok(q) => q,
            Err(e) => {
                set(
                    String::new(),
                    format!("Server: {server}{back}"),
                    format!("{e}. Retrying…"),
                );
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        };
        eprintln!("QUICKCONNECT_CODE={}", qc.code);
        set(
            format!("Sign in code:  {}", qc.code),
            format!("On a phone or computer signed in to Jellyfin as the user to add, open Settings → Quick Connect and enter the code above.\n\nServer: {server}{back}"),
            String::new(),
        );
        // codes expire after a few minutes: ask for a new one then
        for _ in 0..100 {
            std::thread::sleep(Duration::from_secs(3));
            match qc.poll() {
                Ok(Some((auth, name))) => {
                    let mut store = profiles::Store::load().unwrap_or_default();
                    store.add(&auth, &name);
                    if let Err(e) = store.save() {
                        set(
                            String::new(),
                            String::new(),
                            format!("Can't save the login: {e}"),
                        );
                        return;
                    }
                    eprintln!("signed in; restarting");
                    profiles::restart(&["--user", &auth.user_id]);
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    });
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_screen_steps() {
        // a server address ends the setup, whatever the network
        assert_eq!(
            setup_step("http://host:8096", "HOTSPOT", None),
            SetupStep::Done
        );
        // no known Wi-Fi: the hotspot's code first
        assert_eq!(
            setup_step("", "HOTSPOT", Some("10.41.0.1")),
            SetupStep::Wifi
        );
        // on a network: the phone's code
        assert_eq!(
            setup_step("", "CONNECTED", Some("192.0.2.7")),
            SetupStep::Phone
        );
        assert_eq!(setup_step("", "", None), SetupStep::NoNetwork);
    }
}
