//! The Settings screen (gear in the top bar). Slint draws it and moves the focus; this
//! module builds the rows from the settings file and the daemon's status, runs the
//! overlays (list picker, PIN pad, text entry, confirmation) and sends every change
//! through the settings daemon on a worker thread.
//!
//! Sections: rows come from `build_rows`; the Network section lives in `net`
//! (settings_ui/net.rs), Task 6 adds the language rows to Display & audio.

mod lang;
mod net;

use crate::settings::{self, DaemonError, Settings};
use crate::{App, SettingRow};
use serde::Deserialize;
use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

pub const SECTIONS: [&str; 4] = ["Network", "Playback", "Display & audio", "System"];
const NETWORK: usize = 0;
const PLAYBACK: usize = 1;
const DISPLAY: usize = 2;
const SYSTEM: usize = 3;

const SHORT: Duration = Duration::from_secs(5);
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(40);
/// A choice or toggle is sent this long after its last step (one request per burst).
const SEND_DELAY: Duration = Duration::from_millis(500);
const PICKER_ROWS: usize = 9;

// ---- choices ---------------------------------------------------------------------------

/// A choice row's stored values in screen order (Left = previous) and their labels.
struct Choice {
    key: &'static str,
    values: &'static [f64],
    labels: &'static [&'static str],
    int: bool,
}

static CHOICES: [Choice; 4] = [
    Choice {
        key: "playback.max_bitrate",
        values: &[0.0, 40e6, 20e6, 10e6, 6e6, 3e6],
        labels: &[
            "No limit",
            "40 Mbit/s",
            "20 Mbit/s",
            "10 Mbit/s",
            "6 Mbit/s",
            "3 Mbit/s",
        ],
        int: true,
    },
    Choice {
        key: "playback.max_height",
        values: &[2160.0, 1080.0, 720.0, 480.0],
        labels: &["4K", "1080p", "720p", "480p"],
        int: true,
    },
    Choice {
        key: "display.output_max_height",
        values: &[2160.0, 1080.0, 720.0],
        labels: &["4K", "1080p", "720p"],
        int: true,
    },
    Choice {
        key: "display.subtitle_scale",
        values: &[0.8, 1.0, 1.25, 1.5],
        labels: &["Small", "Normal", "Large", "Huge"],
        int: false,
    },
];

fn choice(key: &str) -> Option<&'static Choice> {
    // the per-network choices offer the Playback values
    let key = match key {
        "net.max_bitrate" => "playback.max_bitrate",
        "net.max_height" => "playback.max_height",
        k => k,
    };
    CHOICES.iter().find(|c| c.key == key)
}

impl Choice {
    fn index_of(&self, v: f64) -> Option<usize> {
        self.values.iter().position(|x| (x - v).abs() < 1e-3)
    }
    /// The value as the daemon expects it: integers stay integers.
    fn json(&self, i: usize) -> Value {
        if self.int {
            json!(self.values[i] as i64)
        } else {
            json!(self.values[i])
        }
    }
}

/// What a stored value shows: the choice's own label, else the raw number.
pub fn value_label(key: &str, v: f64) -> String {
    choice(key)
        .and_then(|c| c.index_of(v).map(|i| c.labels[i].to_string()))
        .unwrap_or_else(|| format!("{v}"))
}

/// Index after one step, clamped at both ends (no wrap).
pub fn step_choice<T>(items: &[T], idx: usize, delta: i32) -> usize {
    let last = items.len().saturating_sub(1) as i64;
    (idx as i64 + delta as i64).clamp(0, last) as usize
}

/// First visible picker item: the selection stays in view, the window never runs past the end.
pub fn picker_top(len: usize, sel: usize) -> usize {
    sel.saturating_sub(4).min(len.saturating_sub(PICKER_ROWS))
}

/// The stored value of a choice or toggle key, as JSON. The "here" keys are the current
/// Wi-Fi network's effective limits.
fn stored(s: &Settings, ssid: Option<&str>, key: &str) -> Option<Value> {
    Some(match key {
        "playback.max_bitrate" => json!(s.playback.max_bitrate),
        "playback.max_height" => json!(s.playback.max_height),
        "playback.hdr" => json!(s.playback.hdr),
        "playback.prefer_original" => json!(s.playback.prefer_original),
        "display.output_max_height" => json!(s.display.output_max_height),
        "display.subtitle_scale" => json!(s.display.subtitle_scale as f64),
        "display.audio_passthrough" => json!(s.display.audio_passthrough),
        "net.max_bitrate" => json!(settings::effective(s, ssid).max_bitrate),
        "net.max_height" => json!(settings::effective(s, ssid).max_height),
        _ => return None,
    })
}

// ---- rows --------------------------------------------------------------------------------

/// The daemon's `get` status, the parts the screen shows. Missing parts stay empty.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Status {
    pub wifi: WifiStatus,
    pub macs: Macs,
    pub tailscale: Tailscale,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WifiStatus {
    pub state: String,
    pub ssid: String,
    /// "usb" | "builtin"
    pub radio: Option<String>,
    pub signal_dbm: Option<i32>,
    pub ip: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Macs {
    pub builtin: Option<String>,
    pub usb: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Tailscale {
    pub state: String,
    pub name: Option<String>,
    pub ip: Option<String>,
}

fn tailscale_label(t: &Tailscale) -> String {
    match (t.state.as_str(), &t.name) {
        ("connected", Some(name)) => format!("Connected as {name}"),
        ("connected", None) => "Connected".into(),
        ("signed_out", _) => "Signed out".into(),
        ("off", _) => "Off".into(),
        ("not_installed", _) => "Not installed".into(),
        ("", _) => "…".into(), // status not read yet
        (other, _) => other.into(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    pub kind: &'static str,
    pub label: String,
    pub value: String,
    pub locked: bool,
}

fn row(key: &str, kind: &'static str, label: &str, value: impl Into<String>, locked: bool) -> Row {
    Row {
        key: key.into(),
        kind,
        label: label.into(),
        value: value.into(),
        locked,
    }
}

/// Everything the rows are built from.
pub struct View<'a> {
    pub settings: &'a Settings,
    /// Values stepped on screen but not yet confirmed by the daemon.
    pub pending: &'a HashMap<String, Value>,
    pub status: &'a Status,
    /// A TV resolution change was saved: offer "Restart now".
    pub restart_offered: bool,
    pub net: &'a net::NetState,
    pub lang: &'a lang::LangState,
    /// The "Check for updates" row's value: "", "Checking…", "Up to date", "PiFin x available".
    pub update: &'a str,
}

pub fn build_rows(section: usize, v: &View) -> Vec<Row> {
    let ssid = net::ssid(&v.status.wifi);
    let val = |key: &str| {
        v.pending
            .get(key)
            .cloned()
            .or_else(|| stored(v.settings, ssid, key))
            .unwrap_or(Value::Null)
    };
    let choice_row = |key: &str, label: &str| {
        row(
            key,
            "choice",
            label,
            value_label(key, val(key).as_f64().unwrap_or(0.0)),
            false,
        )
    };
    let toggle_row = |key: &str, label: &str| {
        row(
            key,
            "toggle",
            label,
            if val(key).as_bool() == Some(true) {
                "On"
            } else {
                "Off"
            },
            false,
        )
    };
    // with a PIN set, every sensitive row shows the lock
    let pin = v.settings.system.pin_set;
    match section {
        PLAYBACK => vec![
            choice_row("playback.max_bitrate", "Max bitrate"),
            choice_row("playback.max_height", "Max resolution"),
            toggle_row("playback.hdr", "4K & HDR"),
            toggle_row("playback.prefer_original", "Prefer original"),
        ],
        DISPLAY => {
            let mut r = vec![choice_row("display.output_max_height", "TV resolution")];
            if v.restart_offered {
                r.push(row("display.restart", "action", "Restart now", "", false));
            }
            r.push(choice_row("display.subtitle_scale", "Subtitle size"));
            r.push(toggle_row("display.audio_passthrough", "Audio passthrough"));
            r.extend(lang::rows(v.lang));
            r
        }
        SYSTEM => {
            let (sys, st) = (&v.settings.system, v.status);
            let url = if sys.jellyfin_url.is_empty() {
                "Not set"
            } else {
                sys.jellyfin_url.as_str()
            };
            let mut r = vec![
                row("system.jellyfin_url", "text", "Jellyfin server", url, pin),
                row(
                    "system.tailscale",
                    "action",
                    "Tailscale",
                    tailscale_label(&st.tailscale),
                    pin,
                ),
                row(
                    "secrets.hotspot_pass",
                    "text",
                    "Setup hotspot password",
                    "••••••••",
                    pin,
                ),
                row(
                    "system.pin",
                    "action",
                    "PIN",
                    if pin { "On" } else { "Off" },
                    pin,
                ),
                row("system.pair", "action", "Phone remote", "Show code", false),
                row(
                    "system.about",
                    "info",
                    "About",
                    format!("PiFin {}", env!("CARGO_PKG_VERSION")),
                    false,
                ),
                row(
                    "system.update",
                    "action",
                    "Check for updates",
                    v.update,
                    false,
                ),
                row(
                    "system.ip",
                    "info",
                    "Address",
                    st.wifi.ip.as_deref().unwrap_or("—"),
                    false,
                ),
            ];
            if let Some(ip) = &st.tailscale.ip {
                r.push(row(
                    "system.ts_ip",
                    "info",
                    "Tailscale address",
                    ip.as_str(),
                    false,
                ));
            }
            r.push(row(
                "system.mac_builtin",
                "info",
                "MAC (built-in)",
                st.macs.builtin.as_deref().unwrap_or("—"),
                false,
            ));
            if let Some(mac) = &st.macs.usb {
                r.push(row(
                    "system.mac_usb",
                    "info",
                    "MAC (USB)",
                    mac.as_str(),
                    false,
                ));
            }
            r.push(row("system.reboot", "action", "Restart PiFin", "", false));
            r.push(row("system.factory", "action", "Factory reset", "", pin));
            r
        }
        NETWORK => net::rows(v, &choice_row),
        _ => vec![],
    }
}

/// The row Up/Down/Right lands on: the next one in `dir` that is not "info", else stay.
/// From the section list (-1) going forward that is the first such row.
pub fn next_row(kinds: &[&str], cur: i32, dir: i32) -> i32 {
    let mut i = cur + dir;
    while i >= 0 && (i as usize) < kinds.len() {
        if kinds[i as usize] != "info" {
            return i;
        }
        i += dir;
    }
    cur
}

/// Keep the focus on a usable row after the rows changed (a row appeared or went away).
fn valid_focus(kinds: &[&str], row: i32) -> i32 {
    if row < 0 || kinds.is_empty() {
        return -1;
    }
    let r = row.min(kinds.len() as i32 - 1);
    if kinds[r as usize] != "info" {
        return r;
    }
    let up = next_row(kinds, r, -1);
    if up != r {
        up
    } else {
        next_row(kinds, -1, 1)
    }
}

// ---- PIN pad and text entry ----------------------------------------------------------------

#[derive(Debug, Default)]
pub struct PinEntry {
    pub digits: String,
    pub error: String,
}

#[derive(Debug, PartialEq)]
pub enum PadAction {
    None,
    Submit(String),
    Cancel,
}

impl PinEntry {
    pub fn press(&mut self, key: &str) -> PadAction {
        self.error.clear();
        match key {
            "del" => {
                self.digits.pop();
            }
            "cancel" => return PadAction::Cancel,
            "ok" if self.digits.len() < 4 => self.error = "Enter 4 to 8 digits.".into(),
            "ok" => return PadAction::Submit(self.digits.clone()),
            d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && self.digits.len() < 8 => {
                self.digits.push_str(d)
            }
            _ => {}
        }
        PadAction::None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Layer {
    #[default]
    Lower,
    Upper,
    Symbols,
}

const SYMBOLS: &str = ".-_@:/!?#$%&*+=(),;'\"~^[]{}<>|\\`";

pub fn te_keys(layer: Layer) -> Vec<String> {
    let chars: Vec<String> = match layer {
        Layer::Lower => ('a'..='z').map(String::from).collect(),
        Layer::Upper => ('A'..='Z').map(String::from).collect(),
        Layer::Symbols => SYMBOLS.chars().map(String::from).collect(),
    };
    let tail = ["shift", "sym", "space", "delete", "done"];
    chars
        .into_iter()
        .chain(('0'..='9').map(String::from))
        .chain(tail.iter().map(|s| s.to_string()))
        .collect()
}

#[derive(Debug, Default)]
pub struct TextEntry {
    pub value: String,
    pub layer: Layer,
}

#[derive(Debug, PartialEq)]
pub enum TeAction {
    None,
    Done(String),
    Cancel,
}

impl TextEntry {
    pub fn press(&mut self, key: &str) -> TeAction {
        match key {
            "shift" => {
                self.layer = if self.layer == Layer::Upper {
                    Layer::Lower
                } else {
                    Layer::Upper
                }
            }
            "sym" => {
                self.layer = if self.layer == Layer::Symbols {
                    Layer::Lower
                } else {
                    Layer::Symbols
                }
            }
            "space" => self.value.push(' '),
            "delete" => {
                self.value.pop();
            }
            "done" => return TeAction::Done(self.value.clone()),
            "cancel" => return TeAction::Cancel,
            k => self.value.push_str(k),
        }
        TeAction::None
    }
}

/// The setup hotspot password, checked before it is sent (the daemon checks it again).
pub fn check_hotspot_pass(p: &str) -> Result<(), &'static str> {
    if (8..=63).contains(&p.chars().count()) {
        Ok(())
    } else {
        Err("The password needs 8 to 63 characters.")
    }
}

// ---- screen state ----------------------------------------------------------------------------

/// The step a PIN check unlocks.
#[derive(Clone, Copy, Debug)]
enum Gated {
    Url,
    Tailscale,
    Hotspot,
    Factory,
}

#[derive(Clone, Debug)]
enum PadFor {
    Gate(Gated),
    NewPin { old: Option<String> },
    RepeatPin { old: Option<String>, first: String },
    CurrentForChange,
    CurrentForRemove,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PickFor {
    LangAudio,
    LangSub,
    LangMode,
    Choice(&'static str),
    PinMenu,
    Saved,
    Scan,
}

#[derive(Clone, Debug)]
enum TeFor {
    Url,
    Tailscale,
    Hotspot,
    WifiName,
    /// `open_ok`: an empty password joins an open network ("Other network…" only).
    WifiPass {
        ssid: String,
        open_ok: bool,
    },
}

#[derive(Clone, Debug)]
enum ConfirmFor {
    Reboot,
    Factory(Option<String>),
    Forget {
        ssid: String,
        current: bool,
    },
    Join(String),
    /// Install this release (only when the user asks).
    Update(String),
}

#[derive(Default)]
enum Overlay {
    #[default]
    None,
    Picker {
        title: String,
        items: Vec<String>,
        sel: usize,
        purpose: PickFor,
    },
    Pad {
        title: String,
        entry: PinEntry,
        purpose: PadFor,
    },
    // `pin`: the PIN checked for this one op, dropped with the overlay
    Text {
        title: String,
        entry: TextEntry,
        purpose: TeFor,
        pin: Option<String>,
    },
    Confirm(ConfirmFor),
    /// The phone-remote panel: QR code with the token, the address as text, a note.
    Pair {
        qr: Option<slint::Image>,
        url: String,
        note: String,
    },
}

#[derive(Default)]
struct Ui {
    status: Status,
    pending: HashMap<String, Value>,
    timers: HashMap<String, slint::Timer>,
    restart_offered: bool,
    overlay: Overlay,
    /// A PIN check is out: the pad waits for its answer.
    busy: bool,
    message: String,
    net: net::NetState,
    lang: lang::LangState,
    update: String,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

fn error_text(e: &DaemonError) -> String {
    format!(
        "!{}",
        if e.message.is_empty() {
            "Something went wrong."
        } else {
            e.message.as_str()
        }
    )
}

/// Push the rows and the overlay to Slint.
fn sync(app: &App, u: &Ui) {
    let s = settings::current();
    let section = app
        .get_settings_section()
        .clamp(0, SECTIONS.len() as i32 - 1) as usize;
    let view = View {
        settings: &s,
        pending: &u.pending,
        status: &u.status,
        restart_offered: u.restart_offered,
        net: &u.net,
        lang: &u.lang,
        update: &u.update,
    };
    let rows = build_rows(section, &view);
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind).collect();
    app.set_settings_row(valid_focus(&kinds, app.get_settings_row()));
    let model: Vec<SettingRow> = rows
        .into_iter()
        .map(|r| SettingRow {
            key: r.key.into(),
            kind: r.kind.into(),
            label: r.label.into(),
            value: r.value.into(),
            locked: r.locked,
        })
        .collect();
    app.set_settings_rows(ModelRc::new(VecModel::from(model)));
    app.set_settings_message(u.message.clone().into());

    app.set_picker_show(matches!(u.overlay, Overlay::Picker { .. }));
    app.set_pad_show(matches!(u.overlay, Overlay::Pad { .. }));
    app.set_te_show(matches!(u.overlay, Overlay::Text { .. }));
    app.set_confirm_show(matches!(u.overlay, Overlay::Confirm(_)));
    app.set_pair_show(matches!(u.overlay, Overlay::Pair { .. }));
    match &u.overlay {
        Overlay::None => {}
        Overlay::Pair { qr, url, note } => {
            app.set_pair_has_qr(qr.is_some());
            if let Some(img) = qr {
                app.set_pair_qr(img.clone());
            }
            app.set_pair_url(url.into());
            app.set_pair_note(note.into());
        }
        Overlay::Picker {
            title, items, sel, ..
        } => {
            let top = picker_top(items.len(), *sel);
            let window: Vec<SharedString> = items
                .iter()
                .skip(top)
                .take(PICKER_ROWS)
                .map(|s| s.into())
                .collect();
            app.set_picker_title(title.into());
            app.set_picker_items(ModelRc::new(VecModel::from(window)));
            app.set_picker_sel((sel - top) as i32);
        }
        Overlay::Pad { title, entry, .. } => {
            app.set_pad_title(title.into());
            app.set_pad_len(entry.digits.len() as i32);
            app.set_pad_error(entry.error.clone().into());
        }
        Overlay::Text { title, entry, .. } => {
            let keys: Vec<SharedString> =
                te_keys(entry.layer).into_iter().map(|k| k.into()).collect();
            app.set_te_focus(app.get_te_focus().min(keys.len() as i32 - 1));
            app.set_te_title(title.into());
            app.set_te_value(entry.value.clone().into());
            app.set_te_keys(ModelRc::new(VecModel::from(keys)));
        }
        Overlay::Confirm(purpose) => {
            let (title, text, yes): (String, String, String) = match purpose {
                ConfirmFor::Reboot => ("Restart PiFin?".into(), "The TV goes dark for about 30 seconds.".into(), "Restart".into()),
                ConfirmFor::Factory(_) => (
                    "Factory reset?".into(),
                    "Removes all settings, sign-ins and saved Wi-Fi networks, then restarts.".into(),
                    "Reset".into(),
                ),
                ConfirmFor::Update(v) => (
                    format!("Install PiFin {v}?"),
                    "PiFin downloads it and restarts when it is done (a minute or two). It never updates while a video plays.".into(),
                    "Install".into(),
                ),
                ConfirmFor::Forget { .. } | ConfirmFor::Join(_) => net::confirm_text(purpose),
            };
            app.set_confirm_title(title.into());
            app.set_confirm_text(text.into());
            app.set_confirm_yes(yes.into());
        }
    }
}

/// Show an overlay; every pad, keyboard and confirmation starts with the focus on its first key.
fn open_overlay(app: &App, u: &mut Ui, ov: Overlay) {
    app.set_pad_focus(0);
    app.set_te_focus(0);
    app.set_confirm_focus(0);
    app.set_pair_focus(0);
    u.overlay = ov;
}

fn open_pad(app: &App, u: &mut Ui, title: &str, purpose: PadFor) {
    open_overlay(
        app,
        u,
        Overlay::Pad {
            title: title.into(),
            entry: PinEntry::default(),
            purpose,
        },
    );
}

fn with_ui(app: &App, f: impl FnOnce(&App, &mut Ui)) {
    UI.with(|cell| {
        let mut u = cell.borrow_mut();
        f(app, &mut u);
        sync(app, &u);
    });
}

/// One daemon request on a worker thread; `done` runs on the UI thread with the answer.
fn send(
    app: &App,
    op: &'static str,
    args: Value,
    timeout: Duration,
    done: impl FnOnce(&App, &mut Ui, Result<Value, DaemonError>) + Send + 'static,
) {
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let res = settings::request(op, args, timeout);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                with_ui(&app, |app, u| done(app, u, res));
            }
        });
    });
}

/// Request args with the PIN added when there is one.
fn with_pin(mut args: Value, pin: Option<String>) -> Value {
    if let (Some(p), Some(m)) = (pin, args.as_object_mut()) {
        m.insert("pin".into(), p.into());
    }
    args
}

fn refresh_status(app: &App) {
    send(app, "get", json!({}), SHORT, |_, u, res| match res {
        Ok(v) => {
            u.status = v
                .get("status")
                .cloned()
                .and_then(|s| serde_json::from_value(s).ok())
                .unwrap_or_default()
        }
        Err(e) => u.message = error_text(&e),
    });
}

// ---- choices and toggles: shown at once, sent after the last step ---------------------------

fn step(app: &App, u: &mut Ui, key: &str, delta: i32) {
    if key == "lang.mode" {
        return lang::step(app, u, delta);
    }
    let s = settings::current();
    let cur = u
        .pending
        .get(key)
        .cloned()
        .or_else(|| stored(&s, net::ssid(&u.status.wifi), key));
    let new = match choice(key) {
        Some(c) => {
            let idx = cur
                .as_ref()
                .and_then(|v| v.as_f64())
                .and_then(|v| c.index_of(v))
                .unwrap_or(0);
            c.json(step_choice(c.values, idx, delta))
        }
        // a toggle flips on any step: OK and both arrows change it
        None => match cur.as_ref().and_then(|v| v.as_bool()) {
            Some(on) => json!(!on),
            None => return,
        },
    };
    if cur.as_ref() != Some(&new) {
        set_pending(app, u, key, new);
    }
}

fn set_pending(app: &App, u: &mut Ui, key: &str, value: Value) {
    u.pending.insert(key.to_string(), value);
    let weak = app.as_weak();
    // both per-network choices go out in one network_set
    let k = if net::NET_CHOICES.contains(&key) {
        net::LIMITS_TIMER.to_string()
    } else {
        key.to_string()
    };
    u.timers.entry(k.clone()).or_default().start(
        slint::TimerMode::SingleShot,
        SEND_DELAY,
        move || {
            if let Some(app) = weak.upgrade() {
                with_ui(&app, |app, u| send_pending(app, u, &k));
            }
        },
    );
}

fn send_pending(app: &App, u: &mut Ui, key: &str) {
    if key == net::LIMITS_TIMER {
        return net::send_limits(app, u);
    }
    let Some(value) = u.pending.get(key).cloned() else {
        return;
    };
    let mut changes = serde_json::Map::new();
    changes.insert(key.to_string(), value.clone());
    let k = key.to_string();
    send(
        app,
        "set",
        json!({ "changes": changes }),
        SHORT,
        move |_, u, res| {
            // a newer step of the same key has its own timer: leave its value alone
            if u.pending.get(&k) == Some(&value) {
                u.pending.remove(&k); // shown from the file now, or reverted on an error
            }
            match res {
                Ok(_) if k == "display.output_max_height" => {
                    u.restart_offered = true;
                    u.message = "The new resolution applies after a restart.".into();
                }
                Ok(_) => {}
                Err(e) => u.message = error_text(&e),
            }
        },
    );
}

// ---- actions -------------------------------------------------------------------------------------

fn activate(app: &App, u: &mut Ui, key: &str) {
    if let Some(c) = choice(key) {
        let s = settings::current();
        let cur = u
            .pending
            .get(key)
            .cloned()
            .or_else(|| stored(&s, net::ssid(&u.status.wifi), key));
        let sel = cur
            .and_then(|v| v.as_f64())
            .and_then(|v| c.index_of(v))
            .unwrap_or(0);
        let title = app
            .get_settings_rows()
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.label.to_string())
            .unwrap_or_default();
        let items = c.labels.iter().map(|l| l.to_string()).collect();
        let key = net::NET_CHOICES
            .iter()
            .copied()
            .find(|k| *k == key)
            .unwrap_or(c.key);
        open_overlay(
            app,
            u,
            Overlay::Picker {
                title,
                items,
                sel,
                purpose: PickFor::Choice(key),
            },
        );
        return;
    }
    match key {
        "display.restart" => std::process::exit(0), // pifin-app.service starts a new session
        "system.jellyfin_url" => gated(app, u, Gated::Url),
        "system.tailscale" => gated(app, u, Gated::Tailscale),
        "secrets.hotspot_pass" => gated(app, u, Gated::Hotspot),
        "system.factory" => gated(app, u, Gated::Factory),
        "system.pin" if settings::current().system.pin_set => {
            let items = vec!["Change PIN".to_string(), "Remove PIN".to_string()];
            open_overlay(
                app,
                u,
                Overlay::Picker {
                    title: "PIN".into(),
                    items,
                    sel: 0,
                    purpose: PickFor::PinMenu,
                },
            );
        }
        "system.pin" => open_pad(app, u, "New PIN", PadFor::NewPin { old: None }),
        "system.pair" => open_overlay(app, u, pair_overlay(String::new())),
        "system.reboot" => open_overlay(app, u, Overlay::Confirm(ConfirmFor::Reboot)),
        "system.update" => check_update(app, u),
        k if k.starts_with("net.") => net::activate(app, u, k),
        k if k.starts_with("lang.") => lang::activate(app, u, k),
        _ => {}
    }
}

/// Sensitive rows ask for the PIN first when one is set.
fn gated(app: &App, u: &mut Ui, g: Gated) {
    if settings::current().system.pin_set {
        open_pad(app, u, "Enter PIN", PadFor::Gate(g));
    } else {
        after_gate(app, u, g, None);
    }
}

fn after_gate(app: &App, u: &mut Ui, g: Gated, pin: Option<String>) {
    let text = |title: &str, value: String, purpose| Overlay::Text {
        title: title.into(),
        entry: TextEntry {
            value,
            layer: Layer::Lower,
        },
        purpose,
        pin: pin.clone(),
    };
    let ov = match g {
        Gated::Url => text(
            "Jellyfin server",
            settings::current().system.jellyfin_url.clone(),
            TeFor::Url,
        ),
        Gated::Tailscale => text("Tailscale auth key", String::new(), TeFor::Tailscale),
        Gated::Hotspot => text("Setup hotspot password", String::new(), TeFor::Hotspot),
        Gated::Factory => Overlay::Confirm(ConfirmFor::Factory(pin.clone())),
    };
    open_overlay(app, u, ov);
}

fn picker_ok(app: &App, u: &mut Ui) {
    let Overlay::Picker {
        sel,
        purpose,
        items,
        ..
    } = std::mem::take(&mut u.overlay)
    else {
        return;
    };
    match purpose {
        PickFor::Choice(key) => {
            if let Some(c) = choice(key) {
                set_pending(app, u, key, c.json(sel));
            }
        }
        PickFor::LangAudio | PickFor::LangSub | PickFor::LangMode => {
            lang::picked(app, u, purpose, sel)
        }
        PickFor::PinMenu if sel == 0 => open_pad(app, u, "Current PIN", PadFor::CurrentForChange),
        PickFor::PinMenu => open_pad(app, u, "Current PIN", PadFor::CurrentForRemove),
        PickFor::Saved | PickFor::Scan => net::picked(app, u, purpose, sel, &items),
    }
}

/// A wrong or locked-out PIN keeps the pad open with the reason; anything else closes it.
fn pin_error(u: &mut Ui, e: &DaemonError) {
    let reason = match e.code.as_str() {
        "pin_wrong" => "Wrong PIN.",
        "locked_out" => "Too many wrong PINs. Try again in a minute.",
        _ => {
            u.overlay = Overlay::None;
            u.message = error_text(e);
            return;
        }
    };
    if let Overlay::Pad { entry, .. } = &mut u.overlay {
        entry.digits.clear();
        entry.error = reason.into();
    }
}

fn check_pin(
    app: &App,
    u: &mut Ui,
    pin: String,
    then: impl FnOnce(&App, &mut Ui, String) + Send + 'static,
) {
    u.busy = true;
    send(
        app,
        "pin_check",
        json!({ "pin": pin.clone() }),
        SHORT,
        move |app, u, res| {
            u.busy = false;
            match res {
                Ok(_) => then(app, u, pin),
                Err(e) => pin_error(u, &e),
            }
        },
    );
}

fn pad_key(app: &App, u: &mut Ui, key: &str) {
    if u.busy {
        return;
    }
    let Overlay::Pad { entry, purpose, .. } = &mut u.overlay else {
        return;
    };
    let purpose = purpose.clone();
    match entry.press(key) {
        PadAction::None => {}
        PadAction::Cancel => u.overlay = Overlay::None,
        PadAction::Submit(pin) => match purpose {
            PadFor::Gate(g) => check_pin(app, u, pin, move |app, u, pin| {
                after_gate(app, u, g, Some(pin))
            }),
            PadFor::CurrentForChange => check_pin(app, u, pin, |app, u, pin| {
                open_pad(app, u, "New PIN", PadFor::NewPin { old: Some(pin) })
            }),
            PadFor::CurrentForRemove => {
                u.busy = true;
                send(
                    app,
                    "pin_clear",
                    json!({ "pin": pin }),
                    SHORT,
                    |_, u, res| {
                        u.busy = false;
                        match res {
                            Ok(_) => {
                                u.overlay = Overlay::None;
                                u.message = "PIN removed.".into();
                            }
                            Err(e) => pin_error(u, &e),
                        }
                    },
                );
            }
            PadFor::NewPin { old } => {
                open_pad(app, u, "Repeat PIN", PadFor::RepeatPin { old, first: pin })
            }
            PadFor::RepeatPin { old, first } if first != pin => {
                open_pad(app, u, "New PIN", PadFor::NewPin { old });
                if let Overlay::Pad { entry, .. } = &mut u.overlay {
                    entry.error = "The PINs don't match.".into();
                }
            }
            PadFor::RepeatPin { old, .. } => {
                let mut args = json!({ "pin": pin });
                if let Some(o) = old {
                    args["old_pin"] = o.into();
                }
                u.overlay = Overlay::None;
                send(app, "pin_set", args, SHORT, |_, u, res| {
                    u.message = match res {
                        Ok(_) => "PIN set.".into(),
                        Err(e) => error_text(&e),
                    }
                });
            }
        },
    }
}

fn te_key(app: &App, u: &mut Ui, key: &str) {
    let Overlay::Text {
        entry,
        purpose,
        pin,
        ..
    } = &mut u.overlay
    else {
        return;
    };
    let (purpose, pin) = (purpose.clone(), pin.clone());
    let value = match entry.press(key) {
        TeAction::None => return,
        TeAction::Cancel => {
            u.overlay = Overlay::None;
            return;
        }
        TeAction::Done(v) => v,
    };
    match purpose {
        TeFor::Hotspot if check_hotspot_pass(&value).is_err() => {
            u.message = format!("!{}", check_hotspot_pass(&value).unwrap_err());
        }
        TeFor::Url | TeFor::Hotspot => {
            let key = if matches!(purpose, TeFor::Url) {
                "system.jellyfin_url"
            } else {
                "secrets.hotspot_pass"
            };
            let mut changes = serde_json::Map::new();
            changes.insert(key.into(), value.into());
            u.overlay = Overlay::None;
            u.message = "Saving…".into();
            send(
                app,
                "set",
                with_pin(json!({ "changes": changes }), pin),
                SHORT,
                |_, u, res| {
                    u.message = match res {
                        Ok(_) => "Saved.".into(),
                        Err(e) => error_text(&e),
                    }
                },
            );
        }
        TeFor::WifiName | TeFor::WifiPass { .. } => net::text_done(app, u, purpose, value),
        TeFor::Tailscale => {
            u.overlay = Overlay::None;
            u.message = "Signing in…".into();
            send(
                app,
                "tailscale_login",
                with_pin(json!({ "authkey": value }), pin),
                TAILSCALE_TIMEOUT,
                |app, u, res| match res {
                    Ok(_) => {
                        u.message = "Signed in.".into();
                        refresh_status(app);
                    }
                    Err(e) => u.message = error_text(&e),
                },
            );
        }
    }
}

/// What the phone-remote panel shows: (QR payload, address text). The token stays in the
/// QR code; the text shows the address only.
fn pair_texts(host: &str, token: Option<&str>) -> (String, String) {
    (
        crate::remote::remote_url(host, token),
        crate::remote::remote_url(host, None),
    )
}

/// The panel with the token read from the file now (a Re-pair replaces it).
fn pair_overlay(note: String) -> Overlay {
    match crate::remote::remote_host() {
        Some(host) => {
            let (qr, url) = pair_texts(&host, settings::pair_token().as_deref());
            Overlay::Pair {
                qr: crate::remote::qr_image(&qr),
                url,
                note,
            }
        }
        None => Overlay::Pair {
            qr: None,
            url: "No network address yet.".into(),
            note,
        },
    }
}

/// `again`: the Re-pair button; otherwise Back closes the panel.
fn pair_done(app: &App, u: &mut Ui, again: bool) {
    if !again {
        u.overlay = Overlay::None;
        return;
    }
    if u.busy {
        return;
    }
    u.busy = true;
    send(app, "pair_reset", json!({}), SHORT, |_, u, res| {
        u.busy = false;
        if !matches!(u.overlay, Overlay::Pair { .. }) {
            return; // closed meanwhile
        }
        u.overlay = match res {
            Ok(_) => pair_overlay("Other phones are logged out.".into()),
            Err(e) => pair_overlay(error_text(&e).trim_start_matches('!').to_string()),
        };
    });
}

fn confirm_done(app: &App, u: &mut Ui, yes: bool) {
    let Overlay::Confirm(purpose) = std::mem::take(&mut u.overlay) else {
        return;
    };
    if !yes {
        return;
    }
    let (op, args) = match purpose {
        ConfirmFor::Update(v) => return install_update(app, u, v),
        ConfirmFor::Reboot => ("reboot", json!({})),
        ConfirmFor::Factory(pin) => ("factory_reset", with_pin(json!({}), pin)),
        ConfirmFor::Forget { .. } | ConfirmFor::Join(_) => return net::confirmed(app, u, purpose),
    };
    u.message = "Restarting…".into();
    send(app, op, args, SHORT, |_, u, res| {
        if let Err(e) = res {
            if let Some(text) = restart_error_text(&e) {
                u.message = text;
            }
        }
    });
}

// ---- updates -----------------------------------------------------------------------------------

/// update_check takes up to ~60 s (GitHub over a slow link).
const UPDATE_TIMEOUT: Duration = Duration::from_secs(75);

/// An `update_check` answer -> (row value, the version to offer when there is a newer one).
fn update_outcome(res: &Value) -> (String, Option<String>) {
    let text = |k: &str| {
        res.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let latest = text("latest");
    if res.get("available").and_then(|v| v.as_bool()) == Some(true) && !latest.is_empty() {
        (format!("PiFin {latest} available"), Some(latest))
    } else {
        let current = text("current");
        (
            if current.is_empty() {
                "Up to date".into()
            } else {
                format!("Up to date ({current})")
            },
            None,
        )
    }
}

fn check_update(app: &App, u: &mut Ui) {
    if u.update == "Checking…" {
        return;
    }
    u.update = "Checking…".into();
    send(
        app,
        "update_check",
        json!({}),
        UPDATE_TIMEOUT,
        |app, u, res| match res {
            Ok(v) => {
                let (label, offer) = update_outcome(&v);
                u.update = label;
                if let Some(latest) = offer {
                    open_overlay(app, u, Overlay::Confirm(ConfirmFor::Update(latest)));
                }
            }
            Err(e) => {
                u.update.clear();
                u.message = error_text(&e);
            }
        },
    );
}

fn install_update(app: &App, u: &mut Ui, version: String) {
    u.message = format!("Updating to PiFin {version}… PiFin restarts when it is done.");
    send(app, "update_install", json!({}), SHORT, |_, u, res| {
        if let Err(e) = res {
            if let Some(text) = restart_error_text(&e) {
                u.message = text;
            }
        }
    });
}

/// The text for a failed reboot / factory reset. None when the daemon went away: the box
/// is already restarting, so a lost connection is not an error worth showing.
fn restart_error_text(e: &DaemonError) -> Option<String> {
    (e.code != "unreachable").then(|| error_text(e))
}

// ---- wiring ----------------------------------------------------------------------------------------

/// Open the screen on Playback with the focus on the section list.
pub fn open(app: &App) {
    open_at(app, PLAYBACK);
}

fn open_at(app: &App, section: usize) {
    UI.with(|cell| {
        let mut u = cell.borrow_mut();
        u.overlay = Overlay::None;
        u.message.clear();
        net::reset(&mut u);
    });
    app.set_settings_section(section as i32);
    app.set_settings_row(-1);
    app.set_settings_show(true);
    with_ui(app, |app, u| {
        if section == NETWORK {
            net::section_opened(app, u);
        } else {
            refresh_status(app);
            if section == DISPLAY {
                lang::section_opened(app, u);
            }
        }
    });
}

/// Register the Settings callbacks (the gear's `open-settings` is wired by main) and the
/// Wi-Fi setup screen's OK. `server_probe` measures the Jellyfin server for Test connection.
pub fn install(
    app: &App,
    server_probe: net::Probe,
    client: std::sync::Arc<crate::jellyfin::Client>,
    user: String,
) {
    UI.with(|cell| {
        let mut u = cell.borrow_mut();
        u.net.probe = Some(server_probe);
        u.lang.api = Some(lang::Api { client, user });
    });
    let sections: Vec<SharedString> = SECTIONS.iter().map(|s| SharedString::from(*s)).collect();
    app.set_settings_sections(ModelRc::new(VecModel::from(sections)));

    // every callback below: upgrade the weak handle, then run `f` with the screen state
    fn on<A: 'static>(app: &App, f: impl Fn(&App, &mut Ui, A) + 'static) -> impl Fn(A) + 'static {
        let weak = app.as_weak();
        move |arg| {
            if let Some(app) = weak.upgrade() {
                with_ui(&app, |app, u| f(app, u, arg));
            }
        }
    }
    let close = on(app, |app, u, ()| {
        net::section_left(u);
        app.set_settings_show(false);
    });
    app.on_settings_close(move || close(()));
    app.on_settings_section_changed(on(app, |app, u, section: i32| {
        app.set_settings_row(-1);
        u.message.clear();
        if section == NETWORK as i32 {
            net::section_opened(app, u);
        } else {
            net::section_left(u);
            if section == DISPLAY as i32 {
                lang::section_opened(app, u);
            }
        }
    }));
    // OK on the Wi-Fi setup screen (hotspot): Settings > Network, straight into Add network
    let weak = app.as_weak();
    app.on_wifi_pick(move || {
        if let Some(app) = weak.upgrade() {
            open_at(&app, NETWORK);
            with_ui(&app, net::start_add);
        }
    });
    app.on_settings_move(on(app, |app, _, dir: i32| {
        let rows = app.get_settings_rows();
        let kinds: Vec<SharedString> = rows.iter().map(|r| r.kind).collect();
        let kinds: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        app.set_settings_row(next_row(&kinds, app.get_settings_row(), dir));
    }));
    let step_cb = on(app, |app, u, (key, delta): (SharedString, i32)| {
        step(app, u, &key, delta)
    });
    app.on_settings_step(move |key, delta| step_cb((key, delta)));
    app.on_settings_activate(on(app, |app, u, key: SharedString| activate(app, u, &key)));
    app.on_picker_move(on(app, |_, u, delta: i32| {
        if let Overlay::Picker { items, sel, .. } = &mut u.overlay {
            *sel = step_choice(items, *sel, delta);
        }
    }));
    let ok = on(app, |app, u, ()| picker_ok(app, u));
    app.on_picker_ok(move || ok(()));
    let cancel = on(app, |_, u, ()| u.overlay = Overlay::None);
    app.on_picker_cancel(move || cancel(()));
    app.on_pad_key(on(app, |app, u, key: SharedString| pad_key(app, u, &key)));
    app.on_te_key(on(app, |app, u, key: SharedString| te_key(app, u, &key)));
    app.on_confirm_done(on(app, confirm_done));
    app.on_pair_done(on(app, pair_done));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_hides_a_dropped_connection_only() {
        let gone = DaemonError {
            code: "unreachable".into(),
            message: "Settings service not reachable (no answer).".into(),
        };
        assert_eq!(restart_error_text(&gone), None);
        let wrong = DaemonError {
            code: "pin_wrong".into(),
            message: "Wrong PIN.".into(),
        };
        assert_eq!(restart_error_text(&wrong), Some("!Wrong PIN.".into()));
    }

    #[test]
    fn pin_entry_keeps_8_digits_and_needs_4() {
        let mut p = PinEntry::default();
        for _ in 0..9 {
            assert_eq!(p.press("7"), PadAction::None);
        }
        assert_eq!(p.digits, "77777777");
        assert_eq!(p.press("ok"), PadAction::Submit("77777777".into()));

        let mut p = PinEntry::default();
        for d in ["1", "2", "3"] {
            p.press(d);
        }
        assert_eq!(p.press("ok"), PadAction::None);
        assert_eq!(p.error, "Enter 4 to 8 digits.");

        let mut p = PinEntry::default();
        assert_eq!(p.press("del"), PadAction::None);
        assert_eq!(p.digits, "");
        assert_eq!(p.press("cancel"), PadAction::Cancel);
    }

    #[test]
    fn picker_window_keeps_the_selection_in_view() {
        assert_eq!(picker_top(20, 0), 0);
        assert_eq!(picker_top(20, 10), 6);
        assert_eq!(picker_top(20, 19), 11);
        assert_eq!(picker_top(4, 3), 0);
    }

    fn system_rows(pin_set: bool) -> Vec<Row> {
        let mut s = Settings::default();
        s.system.pin_set = pin_set;
        let status = Status {
            tailscale: Tailscale {
                state: "connected".into(),
                name: Some("pifin".into()),
                ip: Some("192.0.2.10".into()),
            },
            macs: Macs {
                builtin: Some("aa".into()),
                usb: Some("bb".into()),
            },
            ..Default::default()
        };
        build_rows(
            SYSTEM,
            &View {
                settings: &s,
                pending: &HashMap::new(),
                status: &status,
                restart_offered: false,
                net: &net::NetState::default(),
                lang: &lang::LangState::default(),
                update: "",
            },
        )
    }

    #[test]
    fn update_check_offers_only_a_newer_release() {
        let (label, offer) =
            update_outcome(&json!({"current": "0.2.0", "latest": "0.3.0", "available": true}));
        assert_eq!(
            (label.as_str(), offer.as_deref()),
            ("PiFin 0.3.0 available", Some("0.3.0"))
        );
        let (label, offer) =
            update_outcome(&json!({"current": "0.3.0", "latest": "0.3.0", "available": false}));
        assert_eq!((label.as_str(), offer), ("Up to date (0.3.0)", None));
        // "available" without a version to name is not offered
        assert_eq!(update_outcome(&json!({"available": true})).1, None);
    }

    #[test]
    fn system_has_a_check_for_updates_row() {
        let rows = system_rows(false);
        assert!(rows
            .iter()
            .any(|r| r.key == "system.update" && r.kind == "action" && !r.locked));
    }

    #[test]
    fn pair_panel_follows_the_token_file_after_a_reset() {
        let path = std::env::temp_dir().join(format!("pifin-pair-{}", std::process::id()));
        let (a, b) = (
            "0123456789abcdef0123456789abcdef",
            "fedcba9876543210fedcba9876543210",
        );
        std::fs::write(&path, format!("{a}\n")).unwrap();
        let (qr, url) = pair_texts("192.0.2.7", settings::read_pair_token(&path).as_deref());
        assert_eq!(qr, format!("http://192.0.2.7:8080/pair?t={a}"));
        assert_eq!(url, "http://192.0.2.7:8080");
        // pair_reset replaces the file; the panel reads it again
        std::fs::write(&path, format!("{b}\n")).unwrap();
        let (qr, url) = pair_texts("192.0.2.7", settings::read_pair_token(&path).as_deref());
        assert_eq!(qr, format!("http://192.0.2.7:8080/pair?t={b}"));
        assert!(!url.contains(b), "the text shows the address only");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_pin_locks_every_sensitive_row_and_only_those() {
        let sensitive = [
            "system.jellyfin_url",
            "system.tailscale",
            "secrets.hotspot_pass",
            "system.pin",
            "system.factory",
        ];
        let rows = system_rows(true);
        for r in &rows {
            assert_eq!(r.locked, sensitive.contains(&r.key.as_str()), "{}", r.key);
        }
        assert_eq!(rows.iter().filter(|r| r.locked).count(), 5);
        assert!(system_rows(false).iter().all(|r| !r.locked));
        // optional rows show only with a value
        assert!(rows.iter().any(|r| r.key == "system.ts_ip"));
        assert_eq!(
            rows.iter()
                .find(|r| r.key == "system.tailscale")
                .unwrap()
                .value,
            "Connected as pifin"
        );
    }

    #[test]
    fn choice_steps_clamp_at_both_ends() {
        let items = ["a", "b", "c"];
        assert_eq!(step_choice(&items, 0, -1), 0);
        assert_eq!(step_choice(&items, 2, 1), 2);
        assert_eq!(step_choice(&items, 1, 1), 2);
        assert_eq!(step_choice(&items, 1, -1), 0);
    }

    #[test]
    fn values_show_their_labels() {
        assert_eq!(value_label("playback.max_bitrate", 0.0), "No limit");
        assert_eq!(value_label("playback.max_bitrate", 6_000_000.0), "6 Mbit/s");
        assert_eq!(value_label("playback.max_height", 2160.0), "4K");
        assert_eq!(value_label("playback.max_height", 1080.0), "1080p");
        assert_eq!(
            value_label("display.subtitle_scale", 0.8f32 as f64),
            "Small"
        );
    }

    #[test]
    fn up_and_down_skip_info_rows() {
        let kinds = ["text", "info", "info", "action", "info"];
        assert_eq!(next_row(&kinds, -1, 1), 0);
        assert_eq!(next_row(&kinds, 0, 1), 3);
        assert_eq!(next_row(&kinds, 3, 1), 3); // only info below: stay
        assert_eq!(next_row(&kinds, 3, -1), 0);
        assert_eq!(next_row(&kinds, 0, -1), 0); // no wrap, no jump to the section list
        assert_eq!(next_row(&["info"], -1, 1), -1);
    }

    #[test]
    fn hotspot_password_needs_8_to_63_characters() {
        assert!(check_hotspot_pass("1234567").is_err());
        assert!(check_hotspot_pass("12345678").is_ok());
        assert!(check_hotspot_pass(&"x".repeat(63)).is_ok());
        assert!(check_hotspot_pass(&"x".repeat(64)).is_err());
    }

    #[test]
    fn text_entry_switches_layers_and_edits() {
        assert_eq!(te_keys(Layer::Lower).len(), 41);
        assert_eq!(te_keys(Layer::Symbols).len(), 47);
        let mut t = TextEntry::default();
        t.press("a");
        t.press("shift");
        assert_eq!(t.layer, Layer::Upper);
        t.press("B");
        t.press("sym");
        assert_eq!(t.layer, Layer::Symbols);
        t.press("space");
        t.press("delete");
        t.press("@");
        assert_eq!(t.press("done"), TeAction::Done("aB@".into()));
        t.press("shift");
        t.press("shift");
        assert_eq!(t.layer, Layer::Lower);
        assert_eq!(t.press("cancel"), TeAction::Cancel);
    }
}
