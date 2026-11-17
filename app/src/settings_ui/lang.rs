//! Settings > Display & audio: audio language, subtitle language and subtitle mode of the
//! signed-in Jellyfin user. They live on the server (the user's Configuration), so they are
//! read when the section opens and saved on the server. A child module of settings_ui: it
//! reuses the screen's rows and list picker.

use super::{open_overlay, row, step_choice, with_ui, Overlay, PickFor, Row, Ui, SEND_DELAY};
use crate::jellyfin::{Client, LangPrefs};
use crate::App;
use slint::ComponentHandle;
use std::sync::{Arc, Mutex};

/// Stored subtitle modes in screen order, and their labels.
const MODES: [&str; 5] = ["Default", "Always", "OnlyForced", "None", "Smart"];
const MODE_LABELS: [&str; 5] = ["Default", "Always", "Only forced", "None", "Smart"];
const TIMER: &str = "lang.send";

/// The server's languages, fetched once per process.
static CULTURES: Mutex<Option<Vec<(String, String)>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Load {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed,
}

/// Who to talk to: the signed-in user's client and name.
#[derive(Clone)]
pub struct Api {
    pub client: Arc<Client>,
    pub user: String,
}

#[derive(Default)]
pub struct LangState {
    pub load: Load,
    pub prefs: LangPrefs,
    /// The values the server holds: what a failed save goes back to.
    pub saved: LangPrefs,
    pub cultures: Vec<(String, String)>,
    pub api: Option<Api>,
}

/// "Any" for no preference, else the language's name (the code when the server doesn't list it).
fn language_label(cultures: &[(String, String)], code: &str) -> String {
    if code.is_empty() {
        return "Any".into();
    }
    cultures
        .iter()
        .find(|(c, _)| c == code)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| code.to_string())
}

pub fn rows(l: &LangState) -> Vec<Row> {
    let ready = l.load == Load::Ready;
    let show = |f: &dyn Fn() -> String| match l.load {
        Load::Ready => f(),
        Load::Failed => "Server not reachable".into(),
        _ => "Loading…".into(),
    };
    let mode = MODES
        .iter()
        .position(|m| *m == l.prefs.mode)
        .map_or("Default", |i| MODE_LABELS[i]);
    vec![
        row(
            "lang.audio",
            "action",
            "Audio language",
            show(&|| language_label(&l.cultures, &l.prefs.audio)),
            false,
        ),
        row(
            "lang.sub",
            "action",
            "Subtitle language",
            show(&|| language_label(&l.cultures, &l.prefs.subtitle)),
            false,
        ),
        // arrows only once there is something to change
        row(
            "lang.mode",
            if ready { "choice" } else { "action" },
            "Subtitles",
            show(&|| mode.to_string()),
            false,
        ),
    ]
}

/// The section is shown: read the preferences (and, once, the language list).
pub fn section_opened(app: &App, u: &mut Ui) {
    let Some(api) = u.lang.api.clone() else {
        return;
    };
    if u.lang.load != Load::Ready {
        u.lang.load = Load::Loading;
    }
    let weak = app.as_weak();
    std::thread::spawn(move || {
        let cached = CULTURES.lock().ok().and_then(|c| c.clone());
        let res = api.client.lang_prefs().and_then(|(p, _)| {
            let cultures = match cached {
                Some(c) => c,
                None => {
                    let c = api.client.cultures()?;
                    if let Ok(mut g) = CULTURES.lock() {
                        *g = Some(c.clone());
                    }
                    c
                }
            };
            Ok((p, cultures))
        });
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                with_ui(&app, |_, u| match res {
                    Ok((p, c)) => {
                        // a change still waiting to be sent wins over the server's old value
                        if !u.timers.get(TIMER).is_some_and(|t| t.running()) {
                            u.lang.prefs = p.clone();
                        }
                        u.lang.saved = p;
                        u.lang.cultures = c;
                        u.lang.load = Load::Ready;
                    }
                    Err(e) => {
                        eprintln!("LANG load: {e}");
                        u.lang.load = Load::Failed;
                    }
                });
            }
        });
    });
}

/// OK on a language row: the list of languages (with "Any" first), or the mode list.
pub fn activate(app: &App, u: &mut Ui, key: &str) {
    if u.lang.load != Load::Ready {
        return; // loading or unreachable: OK does nothing
    }
    let (title, items, sel, purpose) = match key {
        "lang.mode" => {
            let sel = MODES
                .iter()
                .position(|m| *m == u.lang.prefs.mode)
                .unwrap_or(0);
            (
                "Subtitles",
                MODE_LABELS.iter().map(|s| s.to_string()).collect(),
                sel,
                PickFor::LangMode,
            )
        }
        k => {
            let (cur, purpose, title) = if k == "lang.audio" {
                (&u.lang.prefs.audio, PickFor::LangAudio, "Audio language")
            } else {
                (
                    &u.lang.prefs.subtitle,
                    PickFor::LangSub,
                    "Subtitle language",
                )
            };
            let mut items = vec!["Any".to_string()];
            items.extend(u.lang.cultures.iter().map(|(_, n)| n.clone()));
            let sel = u
                .lang
                .cultures
                .iter()
                .position(|(c, _)| c == cur)
                .map_or(0, |i| i + 1);
            (title, items, sel, purpose)
        }
    };
    open_overlay(
        app,
        u,
        Overlay::Picker {
            title: title.into(),
            items,
            sel,
            purpose,
        },
    );
}

/// Left/Right on the Subtitles row.
pub fn step(app: &App, u: &mut Ui, delta: i32) {
    if u.lang.load != Load::Ready {
        return;
    }
    let cur = MODES
        .iter()
        .position(|m| *m == u.lang.prefs.mode)
        .unwrap_or(0);
    let new = MODES[step_choice(&MODES, cur, delta)];
    change(app, u, |p| p.mode = new.to_string());
}

/// A list pick (`sel` indexes the picker items: 0 = Any).
pub fn picked(app: &App, u: &mut Ui, purpose: PickFor, sel: usize) {
    if purpose == PickFor::LangMode {
        if let Some(m) = MODES.get(sel) {
            change(app, u, |p| p.mode = m.to_string());
        }
        return;
    }
    let code = if sel == 0 {
        String::new()
    } else {
        u.lang
            .cultures
            .get(sel - 1)
            .map(|(c, _)| c.clone())
            .unwrap_or_default()
    };
    if purpose == PickFor::LangAudio {
        change(app, u, |p| p.audio = code);
    } else {
        change(app, u, |p| p.subtitle = code);
    }
}

/// Show the new value at once; send it after the last change.
fn change(app: &App, u: &mut Ui, f: impl FnOnce(&mut LangPrefs)) {
    let before = u.lang.prefs.clone();
    f(&mut u.lang.prefs);
    if u.lang.prefs == before {
        return;
    }
    let weak = app.as_weak();
    u.timers.entry(TIMER.into()).or_default().start(
        slint::TimerMode::SingleShot,
        SEND_DELAY,
        move || {
            if let Some(app) = weak.upgrade() {
                with_ui(&app, send);
            }
        },
    );
}

fn send(app: &App, u: &mut Ui) {
    let Some(api) = u.lang.api.clone() else {
        return;
    };
    let want = u.lang.prefs.clone();
    let weak = app.as_weak();
    std::thread::spawn(move || {
        // re-read right before posting: the server replaces the whole Configuration
        let res = api
            .client
            .lang_prefs()
            .and_then(|(_, cfg)| api.client.set_lang_prefs(cfg, &want));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                with_ui(&app, |_, u| match res {
                    Ok(()) => {
                        u.lang.saved = want;
                        u.message = format!("Saved for {}.", api.user);
                    }
                    Err(e) => {
                        eprintln!("LANG save: {e}");
                        // a newer change has its own send: only revert when nothing is waiting
                        if !u.timers.get(TIMER).is_some_and(|t| t.running()) {
                            u.lang.prefs = u.lang.saved.clone();
                        }
                        u.message = "!Couldn't save on the server.".into();
                    }
                });
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(audio: &str) -> LangState {
        LangState {
            load: Load::Ready,
            prefs: LangPrefs {
                audio: audio.into(),
                subtitle: "fre".into(),
                mode: "Smart".into(),
            },
            cultures: vec![
                ("fra".into(), "French".into()),
                ("fre".into(), "French".into()),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn language_rows_show_any_for_no_preference() {
        let r = rows(&ready(""));
        assert_eq!(r[0].value, "Any");
        assert_eq!(r[1].value, "French");
        assert_eq!(r[2].value, "Smart");
        assert_eq!(r[2].kind, "choice");
        // a null AudioLanguagePreference on the server reads as no preference
        let cfg = serde_json::json!({ "AudioLanguagePreference": null, "SubtitleMode": "Default" });
        assert_eq!(crate::jellyfin::lang_prefs_of(&cfg).audio, "");
    }

    #[test]
    fn rows_say_loading_or_unreachable_and_lock_the_mode_arrows() {
        let mut l = LangState::default();
        l.load = Load::Loading;
        assert!(rows(&l).iter().all(|r| r.value == "Loading…"));
        assert_eq!(rows(&l)[2].kind, "action");
        l.load = Load::Failed;
        assert_eq!(rows(&l)[0].value, "Server not reachable");
    }
}
