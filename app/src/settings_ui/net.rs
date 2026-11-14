//! Settings > Network: the current network, Test connection and the limits it suggests,
//! the limits on this network, saved networks (Forget) and Add network. A child module of
//! settings_ui: it reuses the screen's rows, overlays, keyboard and daemon client.

use super::{
    check_hotspot_pass, error_text, open_overlay, refresh_status, row, send, value_label, with_ui, ConfirmFor, Layer,
    Overlay, PickFor, Row, TeFor, TextEntry, Ui, View, WifiStatus, SHORT,
};
use crate::settings::{self, DaemonError};
use crate::App;
use serde::Deserialize;
use serde_json::{json, Value};
use slint::ComponentHandle;
use std::sync::Arc;
use std::time::Duration;

/// Internet side of Test connection (no auth header; the read stops after 2 s).
pub const INTERNET_URL: &str = "https://speed.cloudflare.com/__down?bytes=25000000";
const SCAN_TIMEOUT: Duration = Duration::from_secs(20);
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
const STATUS_EVERY: Duration = Duration::from_secs(5);
/// Keys in the screen's timer map: the 5 s status refresh, and the one send timer the
/// two per-network choices share (each send carries both values).
pub const STATUS_TIMER: &str = "net.status";
pub const LIMITS_TIMER: &str = "net.limits";
pub const NET_CHOICES: [&str; 2] = ["net.max_bitrate", "net.max_height"];
pub const OTHER: &str = "Other network…";
const TOO_SLOW: &str = "This connection is too slow for smooth video.";
const MBIT: u64 = 1_000_000;

/// The server measurement main hands over: Jellyfin's bitrate test, which also refreshes
/// the link speed Play uses.
pub type Probe = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Test {
    #[default]
    Idle,
    Running,
    Done { internet: Option<u64>, server: Option<u64> },
}

/// The last `wifi_list` answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Saved {
    #[default]
    Unknown,
    Failed,
    Known(Vec<String>),
}

/// One `wifi_scan` entry.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScanEntry {
    pub ssid: String,
    pub signal: i64,
    pub secured: bool,
    pub saved: bool,
}

#[derive(Default)]
pub struct NetState {
    pub test: Test,
    /// Bumped when Settings opens: an answer from an older test is dropped.
    test_gen: u32,
    pub saved: Saved,
    /// The scan the "Add network" picker shows (same order, "Other network…" after it).
    pub scan: Vec<ScanEntry>,
    pub probe: Option<Probe>,
}

// ---- pure parts ------------------------------------------------------------------------------

/// (max_bitrate, max_height) suggested for a measured server speed, and whether the
/// connection is too slow even for the lowest bitrate.
fn suggestion(s: u64) -> ((i64, i32), bool) {
    let budget = 0.8 * s as f64;
    let (bitrate, slow) = if s > 40 * MBIT {
        (0, false)
    } else {
        match [40i64, 20, 10, 6, 3].iter().map(|m| m * MBIT as i64).find(|b| *b as f64 <= budget) {
            Some(b) => (b, false),
            None => (3_000_000, true),
        }
    };
    // direct play of a ~20 Mbit/s 4K file needs 0.8·S >= 20; a converted stream gets 0.6·S
    let height = if s >= 25 * MBIT {
        2160
    } else if s >= 7 * MBIT {
        1080
    } else if s >= 3 * MBIT {
        720
    } else {
        480
    };
    ((bitrate, height), slow)
}

pub fn suggest(server_bps: u64) -> (i64, i32) {
    suggestion(server_bps).0
}

pub fn signal_word(dbm: i32) -> &'static str {
    if dbm >= -60 {
        "strong"
    } else if dbm >= -70 {
        "good"
    } else if dbm >= -80 {
        "weak"
    } else {
        "very weak"
    }
}

/// Mbit/s with one decimal below 10, else whole.
fn mbit(bps: u64) -> String {
    let m = bps as f64 / 1e6;
    if m < 9.95 { format!("{m:.1}") } else { format!("{m:.0}") }
}

fn test_value(t: &Test) -> String {
    let side = |b: &Option<u64>| b.map(|b| format!("{} Mbit/s", mbit(b))).unwrap_or_else(|| "—".into());
    match t {
        Test::Idle => String::new(),
        Test::Running => "Testing…".into(),
        Test::Done { internet, server } => format!("Internet {} · Server {}", side(internet), side(server)),
    }
}

/// The Wi-Fi network the limits belong to: none on Ethernet, in the hotspot or offline.
pub fn ssid(w: &WifiStatus) -> Option<&str> {
    (w.state == "CONNECTED" && !w.ssid.is_empty()).then_some(w.ssid.as_str())
}

pub fn scan_items(entries: &[ScanEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| {
            let mut s = format!("{}   {}%", e.ssid, e.signal);
            if e.saved {
                s.push_str("  (saved)");
            }
            if !e.secured {
                s.push_str("  (open)");
            }
            s
        })
        .chain(std::iter::once(OTHER.to_string()))
        .collect()
}

/// A network name typed for "Other network…", checked before it is sent.
pub fn check_ssid(name: &str) -> Result<(), &'static str> {
    if (1..=32).contains(&name.len()) { Ok(()) } else { Err("The network name needs 1 to 32 characters.") }
}

/// The Network rows; `choice_row` is the screen's own (it shows pending steps).
pub fn rows(v: &View, choice_row: &dyn Fn(&str, &str) -> Row) -> Vec<Row> {
    let w = &v.status.wifi;
    let id = ssid(w);
    let name = if w.state == "HOTSPOT" { "Setup hotspot on" } else { id.unwrap_or("Not connected") };
    let mut r = vec![row("net.ssid", "info", "Network", name, false)];
    if let Some(radio) = &w.radio {
        let radio = match radio.as_str() {
            "usb" => "USB adapter",
            "builtin" => "Built-in",
            other => other,
        };
        r.push(row("net.radio", "info", "Radio", radio, false));
    }
    if let Some(dbm) = w.signal_dbm {
        r.push(row("net.signal", "info", "Signal", format!("{dbm} dBm · {}", signal_word(dbm)), false));
    }
    if let Some(ip) = &w.ip {
        r.push(row("net.ip", "info", "Address", ip.as_str(), false));
    }
    r.push(row("net.test", "action", "Test connection", test_value(&v.net.test), false));
    if let Test::Done { server: Some(s), .. } = v.net.test {
        let value = match id {
            None => "Wi-Fi only".to_string(),
            Some(_) => {
                let (b, h) = suggest(s);
                let (b, h) = (value_label("playback.max_bitrate", b as f64), value_label("playback.max_height", h as f64));
                format!("{b} · {h} · OK to apply")
            }
        };
        r.push(row("net.apply", "action", "Suggested here", value, false));
    }
    if let Some(id) = id {
        let custom = v.settings.networks.contains_key(id);
        r.push(row("net.limits", "action", "Limits on this network", if custom { "Custom" } else { "Same as all networks" }, false));
        if custom {
            r.push(choice_row("net.max_bitrate", "Max bitrate here"));
            r.push(choice_row("net.max_height", "Max resolution here"));
        }
    }
    let saved = match &v.net.saved {
        Saved::Unknown => "…".to_string(),
        Saved::Failed => "—".to_string(),
        Saved::Known(list) => list.len().to_string(),
    };
    r.push(row("net.saved", "action", "Saved networks", saved, false));
    r.push(row("net.add", "action", "Add network", "", false));
    r
}

/// Run both measurements at once (the internet one on its own thread).
pub fn run_probes<A, B>(internet: A, server: B) -> (Option<u64>, Option<u64>)
where
    A: FnOnce() -> Option<u64> + Send + 'static,
    B: FnOnce() -> Option<u64>,
{
    let other = std::thread::spawn(internet);
    let s = server();
    (other.join().ok().flatten(), s)
}

// ---- screen wiring ----------------------------------------------------------------------------

/// Settings opened: forget the last test and saved count (maybe another network now).
pub fn reset(u: &mut Ui) {
    u.net.test = Test::Idle;
    u.net.test_gen += 1;
    u.net.saved = Saved::Unknown;
}

/// The section is shown: read the status and the saved networks, then the status every 5 s.
pub fn section_opened(app: &App, u: &mut Ui) {
    refresh_status(app);
    list_saved(app, false);
    let weak = app.as_weak();
    u.timers.entry(STATUS_TIMER.into()).or_default().start(slint::TimerMode::Repeated, STATUS_EVERY, move || {
        if let Some(app) = weak.upgrade() {
            refresh_status(&app);
        }
    });
}

pub fn section_left(u: &mut Ui) {
    if let Some(t) = u.timers.get(STATUS_TIMER) {
        t.stop();
    }
}

/// An answer may come back after Settings closed or another overlay opened: open a
/// picker only on the bare screen.
fn can_pick(app: &App, u: &Ui) -> bool {
    app.get_settings_show() && matches!(u.overlay, Overlay::None)
}

fn list_saved(app: &App, then_pick: bool) {
    send(app, "wifi_list", json!({}), SHORT, move |app, u, res| match res {
        Ok(v) => {
            let mut names: Vec<String> = Vec::new();
            for p in v.as_array().into_iter().flatten() {
                if let Some(s) = p.get("ssid").and_then(|s| s.as_str()) {
                    if !names.iter().any(|n| n == s) {
                        names.push(s.to_string());
                    }
                }
            }
            u.net.saved = Saved::Known(names.clone());
            if then_pick && can_pick(app, u) {
                if names.is_empty() {
                    u.message = "No saved networks.".into();
                } else {
                    let ov = Overlay::Picker { title: "Saved networks".into(), items: names, sel: 0, purpose: PickFor::Saved };
                    open_overlay(app, u, ov);
                }
            }
        }
        Err(e) => {
            u.net.saved = Saved::Failed;
            u.message = error_text(&e);
        }
    });
}

/// "Add network": scan, then the picker. Also the Wi-Fi setup screen's OK.
pub fn start_add(app: &App, u: &mut Ui) {
    u.message = "Looking for networks…".into();
    send(app, "wifi_scan", json!({}), SCAN_TIMEOUT, |app, u, res| match res {
        Ok(v) => {
            u.net.scan = serde_json::from_value(v).unwrap_or_default();
            u.message.clear();
            if can_pick(app, u) {
                let ov = Overlay::Picker { title: "Add network".into(), items: scan_items(&u.net.scan), sel: 0, purpose: PickFor::Scan };
                open_overlay(app, u, ov);
            }
        }
        Err(e) => u.message = error_text(&e),
    });
}

fn start_test(app: &App, u: &mut Ui) {
    let Some(probe) = u.net.probe.clone() else { return };
    u.net.test = Test::Running;
    let gen = u.net.test_gen;
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let (internet, server) = run_probes(|| crate::jellyfin::measure(INTERNET_URL, None, 2.0), move || probe());
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                with_ui(&app, |_, u| {
                    if u.net.test_gen != gen {
                        return;
                    }
                    u.net.test = Test::Done { internet, server };
                    match server {
                        None => u.message = "!Can't reach the Jellyfin server.".into(),
                        Some(s) if suggestion(s).1 => u.message = TOO_SLOW.into(),
                        Some(_) => {}
                    }
                });
            }
        });
    });
}

fn network_set(app: &App, ssid: String, limits: Value, saved_msg: Option<String>) {
    send(app, "network_set", json!({ "ssid": ssid, "limits": limits }), SHORT, move |_, u, res| match res {
        Ok(_) => {
            if let Some(m) = saved_msg {
                u.message = m;
            }
        }
        Err(e) => u.message = error_text(&e),
    });
}

pub fn activate(app: &App, u: &mut Ui, key: &str) {
    let id = ssid(&u.status.wifi).map(String::from);
    match key {
        "net.test" if u.net.test != Test::Running => start_test(app, u),
        "net.apply" => {
            if let (Test::Done { server: Some(s), .. }, Some(id)) = (&u.net.test, id) {
                let (b, h) = suggest(*s);
                let msg = format!("Limits saved for {id}.");
                network_set(app, id, json!({ "max_bitrate": b, "max_height": h }), Some(msg));
            }
        }
        "net.limits" => {
            let Some(id) = id else { return };
            let s = settings::current();
            if s.networks.contains_key(&id) {
                network_set(app, id, Value::Null, None);
            } else {
                let p = &s.playback;
                network_set(app, id, json!({ "max_bitrate": p.max_bitrate, "max_height": p.max_height }), None);
            }
        }
        "net.saved" => list_saved(app, true),
        "net.add" => start_add(app, u),
        _ => {}
    }
}

/// The two "here" choices, sent together after the last step.
pub fn send_limits(app: &App, u: &mut Ui) {
    let Some(id) = ssid(&u.status.wifi).map(String::from) else {
        for k in NET_CHOICES {
            u.pending.remove(k);
        }
        return;
    };
    let eff = settings::effective(&settings::current(), Some(&id));
    let sent: Vec<Option<Value>> = NET_CHOICES.iter().map(|k| u.pending.get(*k).cloned()).collect();
    let b = sent[0].clone().unwrap_or(json!(eff.max_bitrate));
    let h = sent[1].clone().unwrap_or(json!(eff.max_height));
    let args = json!({ "ssid": id, "limits": { "max_bitrate": b, "max_height": h } });
    send(app, "network_set", args, SHORT, move |_, u, res: Result<Value, DaemonError>| {
        // a newer step has its own send coming: leave its value alone
        for (k, v) in NET_CHOICES.iter().zip(sent) {
            if v.is_some() && u.pending.get(*k) == v.as_ref() {
                u.pending.remove(*k);
            }
        }
        if let Err(e) = res {
            u.message = error_text(&e);
        }
    });
}

fn text(title: String, purpose: TeFor) -> Overlay {
    Overlay::Text { title, entry: TextEntry { value: String::new(), layer: Layer::Lower }, purpose, pin: None }
}

pub fn picked(app: &App, u: &mut Ui, purpose: PickFor, sel: usize, items: &[String]) {
    match purpose {
        PickFor::Saved => {
            let Some(name) = items.get(sel).cloned() else { return };
            let current = ssid(&u.status.wifi) == Some(name.as_str());
            open_overlay(app, u, Overlay::Confirm(ConfirmFor::Forget { ssid: name, current }));
        }
        PickFor::Scan => match u.net.scan.get(sel).cloned() {
            None => open_overlay(app, u, text("Network name".into(), TeFor::WifiName)),
            Some(e) if e.secured => {
                let title = format!("Password for {}", e.ssid);
                open_overlay(app, u, text(title, TeFor::WifiPass { ssid: e.ssid, open_ok: false }));
            }
            Some(e) => open_overlay(app, u, Overlay::Confirm(ConfirmFor::Join(e.ssid))),
        },
        _ => {}
    }
}

pub fn text_done(app: &App, u: &mut Ui, purpose: TeFor, value: String) {
    match purpose {
        TeFor::WifiName => match check_ssid(&value) {
            Err(m) => u.message = format!("!{m}"),
            Ok(()) => {
                let title = format!("Password for {value}");
                // a hidden network may be open: an empty password joins without one
                open_overlay(app, u, text(title, TeFor::WifiPass { ssid: value, open_ok: true }));
            }
        },
        TeFor::WifiPass { ssid, open_ok } => {
            if open_ok && value.is_empty() {
                return join(app, u, ssid, String::new());
            }
            match check_hotspot_pass(&value) {
                Err(m) => u.message = format!("!{m}"),
                Ok(()) => join(app, u, ssid, value),
            }
        }
        _ => {}
    }
}

/// `wifi_add`, then Settings closes: comitup's CONNECTING state brings the Wi-Fi screen.
fn join(app: &App, u: &mut Ui, ssid: String, psk: String) {
    u.overlay = Overlay::None;
    u.message = format!("Joining {ssid}…");
    send(app, "wifi_add", json!({ "ssid": ssid, "psk": psk }), JOIN_TIMEOUT, |app, u, res| match res {
        Ok(_) => {
            u.overlay = Overlay::None;
            u.message.clear();
            section_left(u);
            app.set_settings_show(false);
        }
        Err(e) => u.message = error_text(&e),
    });
}

pub fn confirm_text(purpose: &ConfirmFor) -> (String, String, String) {
    match purpose {
        ConfirmFor::Forget { ssid, current } => {
            let mut text = "PiFin won't join it again until you add it.".to_string();
            if *current {
                text.push_str(" PiFin will disconnect now.");
            }
            (format!("Forget {ssid}?"), text, "Forget".into())
        }
        ConfirmFor::Join(ssid) => (format!("Join {ssid}?"), String::new(), "Join".into()),
        _ => Default::default(),
    }
}

/// "Yes" on a Network confirmation.
pub fn confirmed(app: &App, u: &mut Ui, purpose: ConfirmFor) {
    match purpose {
        ConfirmFor::Forget { ssid, .. } => {
            send(app, "wifi_forget", json!({ "ssid": ssid.clone() }), SHORT, move |app, u, res| match res {
                Ok(_) => {
                    u.message = format!("{ssid} forgotten.");
                    list_saved(app, false);
                }
                Err(e) => u.message = error_text(&e),
            });
        }
        ConfirmFor::Join(ssid) => join(app, u, ssid, String::new()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::{build_rows, Status, NETWORK};
    use super::*;
    use crate::settings::{NetLimits, Settings};
    use std::collections::HashMap;
    use std::sync::mpsc;

    #[test]
    fn suggestion_follows_the_server_speed() {
        let m = |x: u64| x * MBIT;
        assert_eq!(suggest(m(50)), (0, 2160));
        assert_eq!(suggest(m(30)), (20_000_000, 2160));
        assert_eq!(suggest(m(12)), (6_000_000, 1080));
        assert_eq!(suggest(m(7)), (3_000_000, 1080));
        assert_eq!(suggest(m(5)), (3_000_000, 720));
        assert_eq!(suggest(m(2)), (3_000_000, 480));
        assert!(suggestion(m(2)).1 && !suggestion(m(5)).1);
    }

    #[test]
    fn signal_words_at_the_edges() {
        assert_eq!(signal_word(-60), "strong");
        assert_eq!(signal_word(-61), "good");
        assert_eq!(signal_word(-80), "weak");
        assert_eq!(signal_word(-81), "very weak");
    }

    #[test]
    fn test_result_shows_both_sides() {
        let t = Test::Done { internet: Some(85_400_000), server: Some(7_260_000) };
        assert_eq!(test_value(&t), "Internet 85 Mbit/s · Server 7.3 Mbit/s");
        assert_eq!(test_value(&Test::Done { internet: None, server: Some(12_000_000) }), "Internet — · Server 12 Mbit/s");
        assert_eq!(test_value(&Test::Running), "Testing…");
    }

    fn wifi(state: &str, ssid: &str) -> WifiStatus {
        WifiStatus { state: state.into(), ssid: ssid.into(), ..Default::default() }
    }

    fn net_rows(s: &Settings, w: WifiStatus, test: Test) -> Vec<Row> {
        let status = Status { wifi: w, ..Default::default() };
        let net = NetState { test, ..Default::default() };
        build_rows(NETWORK, &View { settings: s, pending: &HashMap::new(), status: &status, restart_offered: false, net: &net, lang: &crate::settings_ui::lang::LangState::default(), update: "" })
    }

    fn find<'a>(rows: &'a [Row], key: &str) -> Option<&'a Row> {
        rows.iter().find(|r| r.key == key)
    }

    #[test]
    fn without_a_wifi_network_there_are_no_per_network_limits() {
        let done = Test::Done { internet: Some(50_000_000), server: Some(30_000_000) };
        // Ethernet or offline: suggestion shown but not appliable, no limits row
        let rows = net_rows(&Settings::default(), wifi("", ""), done.clone());
        assert!(find(&rows, "net.limits").is_none());
        assert_eq!(find(&rows, "net.apply").unwrap().value, "Wi-Fi only");
        assert_eq!(find(&rows, "net.ssid").unwrap().value, "Not connected");
        // hotspot: same, with its own name
        let rows = net_rows(&Settings::default(), wifi("HOTSPOT", "Home"), done.clone());
        assert!(find(&rows, "net.limits").is_none());
        assert_eq!(find(&rows, "net.ssid").unwrap().value, "Setup hotspot on");
        // on Wi-Fi: the suggestion can be applied
        let rows = net_rows(&Settings::default(), wifi("CONNECTED", "Home"), done);
        assert_eq!(find(&rows, "net.apply").unwrap().value, "20 Mbit/s · 4K · OK to apply");
        assert_eq!(find(&rows, "net.limits").unwrap().value, "Same as all networks");
        assert!(find(&rows, "net.max_bitrate").is_none());
        // no test with a server result yet: no suggestion row
        let rows = net_rows(&Settings::default(), wifi("CONNECTED", "Home"), Test::Done { internet: Some(1), server: None });
        assert!(find(&rows, "net.apply").is_none());
    }

    #[test]
    fn a_custom_network_shows_its_own_limits() {
        let mut s = Settings::default();
        s.playback.max_bitrate = 0;
        s.playback.max_height = 1080;
        s.networks.insert("Home".into(), NetLimits { max_bitrate: Some(6_000_000), ..Default::default() });
        let rows = net_rows(&s, wifi("CONNECTED", "Home"), Test::Idle);
        assert_eq!(find(&rows, "net.limits").unwrap().value, "Custom");
        // a key the entry leaves out shows the global value
        assert_eq!(find(&rows, "net.max_bitrate").unwrap().value, "6 Mbit/s");
        assert_eq!(find(&rows, "net.max_height").unwrap().value, "1080p");
        assert_eq!(find(&rows, "net.max_height").unwrap().kind, "choice");
        // another network uses the global limits
        let rows = net_rows(&s, wifi("CONNECTED", "Cafe"), Test::Idle);
        assert_eq!(find(&rows, "net.limits").unwrap().value, "Same as all networks");
        assert!(find(&rows, "net.max_height").is_none());
    }

    #[test]
    fn info_rows_show_only_with_a_value() {
        let w = WifiStatus {
            radio: Some("usb".into()),
            signal_dbm: Some(-67),
            ip: Some("192.0.2.5".into()),
            ..wifi("CONNECTED", "Home")
        };
        let rows = net_rows(&Settings::default(), w, Test::Idle);
        assert_eq!(find(&rows, "net.radio").unwrap().value, "USB adapter");
        assert_eq!(find(&rows, "net.signal").unwrap().value, "-67 dBm · good");
        assert_eq!(find(&rows, "net.saved").unwrap().value, "…");
        let rows = net_rows(&Settings::default(), wifi("CONNECTED", "Home"), Test::Idle);
        assert!(find(&rows, "net.radio").is_none() && find(&rows, "net.signal").is_none() && find(&rows, "net.ip").is_none());
    }

    #[test]
    fn scan_items_flag_saved_and_open_networks() {
        let e = |ssid: &str, signal, secured, saved| ScanEntry { ssid: ssid.into(), signal, secured, saved };
        let items = scan_items(&[e("Home", 80, true, true), e("Cafe", 45, false, false), e("Lab", 30, true, false)]);
        assert_eq!(items, ["Home   80%  (saved)", "Cafe   45%  (open)", "Lab   30%", "Other network…"]);
        assert_eq!(scan_items(&[]), ["Other network…"]);
    }

    #[test]
    fn network_names_need_1_to_32_bytes() {
        assert!(check_ssid("").is_err());
        assert!(check_ssid(&"x".repeat(32)).is_ok());
        assert!(check_ssid(&"é".repeat(17)).is_err()); // 34 bytes
    }

    #[test]
    fn both_measurements_run_at_once() {
        // each probe waits for the other to start: run one after the other, both time out
        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel::<()>();
        let wait = Duration::from_secs(2);
        let internet = move || {
            tx_a.send(()).ok();
            rx_b.recv_timeout(wait).ok().map(|_| 11)
        };
        let server = move || {
            tx_b.send(()).ok();
            rx_a.recv_timeout(wait).ok().map(|_| 22)
        };
        assert_eq!(run_probes(internet, server), (Some(11), Some(22)));
    }
}
