//! The phone remote: its command socket, and the QR code hint shown at boot.

use crate::jellyfin::{Client, Item};
use crate::screens::{show_top, State};
use crate::{settings, App};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const APP_SOCK: &str = "/tmp/pifin-app.sock";

type RemoteFn = Box<dyn Fn(&App, &str, Option<Item>)>;

thread_local! {
    static REMOTE: RefCell<Option<RemoteFn>> = const { RefCell::new(None) };
}

/// Set once the phone-remote hint was dismissed, so a late network doesn't bring it back.
static HINT_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Hides the phone-remote QR hint for good (until no phone is connected any more).
pub(crate) fn hide_remote_hint(app: &App) {
    HINT_OFF.store(true, std::sync::atomic::Ordering::Relaxed);
    app.set_remote_show(false);
}

/// Shows the phone-remote QR hint; false while there is no address yet.
fn show_remote_hint(app: &App) -> bool {
    let Some(host) = remote_host() else {
        return false;
    };
    // the token is read again every time: a Re-pair may have replaced it
    let token = settings::pair_token();
    if let Some(img) = qr_image(&remote_url(&host, token.as_deref())) {
        app.set_remote_qr(img);
        // the text shows the address only; the token stays in the QR code
        app.set_remote_url(remote_url(&host, None).into());
        app.set_remote_show(true);
    }
    true
}

/// The address phones on the same network use to reach us.
pub(crate) fn local_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // documentation address: no packet is sent
    Some(s.local_addr().ok()?.ip())
}

/// The phone remote's address for the QR code: with the pairing token the scan pairs the
/// phone; without one (no token file yet) the plain address.
pub(crate) fn remote_url(host: &str, token: Option<&str>) -> String {
    match token {
        Some(t) => format!("http://{host}:8080/pair?t={t}"),
        None => format!("http://{host}:8080"),
    }
}

/// The address phones reach the remote at; PIFIN_REMOTE_HOST overrides it (screenshots).
pub(crate) fn remote_host() -> Option<String> {
    std::env::var("PIFIN_REMOTE_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| local_ip().map(|ip| ip.to_string()))
}

/// A QR code as a 1-pixel-per-module image (scaled up by the UI, pixelated).
pub(crate) fn qr_image(text: &str) -> Option<slint::Image> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let n = code.width() as u32;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(n, n);
    for (px, c) in buf.make_mut_slice().iter_mut().zip(code.to_colors()) {
        let v = if c == qrcode::Color::Dark { 0 } else { 255 };
        *px = Rgba8Pixel {
            r: v,
            g: v,
            b: v,
            a: 255,
        };
    }
    Some(slint::Image::from_rgba8(buf))
}

/// Run a remote command on the UI thread (also used to replay a Play once the full
/// item is known).
pub(crate) fn dispatch(app: &App, cmd: &str, item: Option<Item>) {
    REMOTE.with(|r| {
        if let Some(f) = r.borrow().as_ref() {
            f(app, cmd, item);
        }
    });
}

/// Commands from the phone remote on /tmp/pifin-app.sock, one line each.
pub(crate) fn listen(
    app: &App,
    client: &Arc<Client>,
    state: &Rc<RefCell<State>>,
    open_item: &Rc<dyn Fn(Item, bool)>,
    open_search: &Rc<dyn Fn(String)>,
) {
    let _ = std::fs::remove_file(APP_SOCK);
    match std::os::unix::net::UnixListener::bind(APP_SOCK) {
        Ok(listener) => {
            let weak = app.as_weak();
            let client = client.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for conn in listener.incoming().flatten() {
                    let mut line = String::new();
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                    if std::io::BufReader::new(conn).read_line(&mut line).is_err() {
                        continue;
                    }
                    let line = line.trim_end_matches(['\r', '\n']).to_string();
                    let mut parts = line.splitn(2, ' ');
                    let (mut cmd, arg) = (
                        parts.next().unwrap_or("").to_string(),
                        parts.next().unwrap_or("").to_string(),
                    );
                    if cmd == "text" {
                        cmd = line.clone();
                    }
                    // items are fetched here, off the UI thread
                    let item = if matches!(cmd.as_str(), "play" | "open") {
                        client.item(&arg).ok()
                    } else {
                        None
                    };
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        dispatch(&app, &cmd, item);
                    });
                }
            });
        }
        Err(e) => eprintln!("command socket {APP_SOCK}: {e}"),
    }
    let state = state.clone();
    let open_item = open_item.clone();
    let open_search = open_search.clone();
    REMOTE.with(|r| {
        *r.borrow_mut() = Some(Box::new(
            move |app: &App, cmd: &str, item: Option<Item>| match cmd {
                "seen" => hide_remote_hint(app),
                // no phone connected any more: the QR code again, so one can pair or reconnect
                "unseen" => {
                    HINT_OFF.store(false, std::sync::atomic::Ordering::Relaxed);
                    show_remote_hint(app);
                }
                "home" => {
                    let mut st = state.borrow_mut();
                    if st.screens.len() > 1 {
                        st.screens.truncate(1);
                        show_top(app, &mut st);
                    }
                }
                "play" | "open" => {
                    if let Some(it) = item {
                        open_item(it, cmd == "play");
                    }
                }
                c if c.starts_with("text ") => open_search(c[5..].to_string()),
                _ => eprintln!("unknown remote command {cmd}"),
            },
        ));
    });
}

/// Boot hint: the phone remote's QR code until a key press or a phone connects.
/// The app starts before the network, so the address is looked up every 2 s
/// (up to 3 min) until there is one, unless the hint was dismissed meanwhile.
pub(crate) fn boot_hint(app: &App) {
    if std::env::var_os("PIFIN_RESTARTED").is_none() {
        let weak = app.as_weak();
        let tries = std::cell::Cell::new(0u32);
        let show = move || -> bool {
            let Some(app) = weak.upgrade() else {
                return true;
            };
            tries.set(tries.get() + 1);
            if HINT_OFF.load(std::sync::atomic::Ordering::Relaxed) || tries.get() > 90 {
                return true;
            }
            show_remote_hint(&app)
        };
        if !show() {
            let timer = Rc::new(slint::Timer::default());
            let t = timer.clone();
            timer.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(2),
                move || {
                    if show() {
                        t.stop();
                    }
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_qr_carries_the_pairing_token() {
        let t = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            remote_url("192.0.2.5", Some(t)),
            format!("http://192.0.2.5:8080/pair?t={t}")
        );
        assert_eq!(remote_url("192.0.2.5", None), "http://192.0.2.5:8080");
    }
}
