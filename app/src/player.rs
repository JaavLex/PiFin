//! The mpv player: starting it for a plan, talking to it over its IPC socket,
//! adding subtitles, and reporting progress to the server.

use crate::home::{fetch_home, save_home};
use crate::jellyfin::{Client, Item};
use crate::playback::{link_bps, plan};
use crate::screens::{fmt_ticks, install_screen, poster_id, with_state};
use crate::{modeswitch, settings, App, Ctx};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where pifin-osd.lua and mpv-input.conf live on the Pi.
const MPV_DATA: &str = "/opt/pifin-remote";

const NOW_FILE: &str = "/tmp/pifin-now.json";

/// Per-playback mpv options from the settings (and, later, the chosen tracks).
struct MpvOpts {
    sub_scale: f32,
    passthrough: bool,
    aid: Option<String>,
    sid: Option<String>,
    /// HDR source played with the 4K & HDR setting on: ask the compositor for HDR output.
    hdr_hint: bool,
}

impl MpvOpts {
    fn from_settings() -> MpvOpts {
        let d = settings::current().display.clone();
        MpvOpts {
            sub_scale: d.subtitle_scale,
            passthrough: d.audio_passthrough,
            aid: None,
            sid: None,
            hdr_hint: false,
        }
    }
}

/// mpv arguments proven on the Pi 4 (0 dropped frames on HEVC 10-bit 1080p).
fn mpv_command(url: &str, start_s: f64, title: &str, opts: &MpvOpts) -> Command {
    let mut c = Command::new("mpv");
    c.args([
        "--fs",
        "--vo=gpu",
        "--gpu-context=wayland",
        "--profile=fast",
        "--hwdec=drm,v4l2m2m-copy,no",
        "--cache=yes",
        "--demuxer-max-bytes=150MiB",
        // weak Wi-Fi: read ahead, and after an underrun wait for a few seconds of
        // buffer instead of stuttering every couple of seconds
        "--demuxer-readahead-secs=20",
        "--cache-pause-wait=4",
        "--osc=no",
        "--osd-level=0",
        "--really-quiet",
    ]);
    c.arg(format!("--script={MPV_DATA}/pifin-osd.lua"));
    c.arg(format!("--input-conf={MPV_DATA}/mpv-input.conf"));
    c.arg("--input-ipc-server=/tmp/pifin-mpv.sock");
    c.arg(format!("--force-media-title={title}"));
    if start_s > 0.0 {
        c.arg(format!("--start={start_s:.1}"));
    }
    if opts.sub_scale != 1.0 {
        c.arg(format!("--sub-scale={:.2}", opts.sub_scale));
    }
    if opts.passthrough {
        c.arg("--audio-spdif=ac3,eac3,dts,dts-hd,truehd");
    }
    if let Some(a) = &opts.aid {
        c.arg(format!("--aid={a}"));
    }
    if let Some(s) = &opts.sid {
        c.arg(format!("--sid={s}"));
    }
    if opts.hdr_hint {
        // mpv >= 0.38 (Pi: 0.40); a no-op where the compositor has no colour management
        c.arg("--target-colorspace-hint=yes");
    }
    c.arg(url);
    c
}

/// Talk to the running mpv over its JSON IPC socket (blocking; used off the UI thread).
fn mpv_get(prop: &str) -> Option<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut s = std::os::unix::net::UnixStream::connect("/tmp/pifin-mpv.sock").ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(format!("{{\"command\":[\"get_property\",\"{prop}\"]}}\n").as_bytes())
        .ok()?;
    let mut lines = BufReader::new(s).lines();
    while let Some(Ok(line)) = lines.next() {
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("event").is_none() {
            return v.get("data").cloned();
        }
    }
    None
}

/// Send a command to the running mpv (blocking; used off the UI thread).
fn mpv_send(args: &[&str]) -> bool {
    use std::io::Write;
    let Ok(mut s) = std::os::unix::net::UnixStream::connect("/tmp/pifin-mpv.sock") else {
        return false;
    };
    let cmd = serde_json::json!({ "command": args });
    s.write_all(format!("{cmd}\n").as_bytes()).is_ok()
}

/// External/extracted subtitles are added once playback runs: the server may take
/// a while to extract them from a big file, and that must not delay the picture.
/// The first extraction of an embedded subtitle can take minutes on a big file, longer
/// than mpv's network timeout, so the app downloads each one itself (long timeout) to a
/// local file and adds that; the URL is the fallback.
fn spawn_sub_loader(
    subs: Vec<(String, String)>,
    select: Option<usize>,
    exited: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    if subs.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into();
        let tag = std::process::id();
        let mut sources = Vec::new();
        for (i, (url, title)) in subs.into_iter().enumerate() {
            if exited.load(Ordering::Relaxed) {
                return;
            }
            let path = format!("/tmp/pifin-sub-{tag}-{i}.{}", sub_ext(&url));
            let t0 = Instant::now();
            let got = agent
                .get(&url)
                .call()
                .map_err(|e| e.to_string())
                .and_then(|mut r| {
                    r.body_mut()
                        .with_config()
                        .limit(32 * 1024 * 1024)
                        .read_to_vec()
                        .map_err(|e| e.to_string())
                });
            let src = match got.and_then(|b| std::fs::write(&path, b).map_err(|e| e.to_string())) {
                Ok(()) => {
                    eprintln!("SUB_FETCH {i} ok {}ms", t0.elapsed().as_millis());
                    path
                }
                Err(e) => {
                    eprintln!("SUB_FETCH {i} failed ({e}) {}", redact(&url));
                    url
                }
            };
            sources.push((src, title));
        }
        for _ in 0..60 {
            if exited.load(Ordering::Relaxed) || mpv_get("time-pos").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        for (i, (src, title)) in sources.into_iter().enumerate() {
            if exited.load(Ordering::Relaxed) {
                return;
            }
            mpv_send(&[
                "sub-add",
                &src,
                if select == Some(i) { "select" } else { "auto" },
                &title,
            ]);
        }
    });
}

/// File extension for a subtitle URL (`.../Stream.srt?...` -> "srt").
fn sub_ext(url: &str) -> &str {
    let path = url.split('?').next().unwrap_or(url);
    match path.rsplit_once('.') {
        Some((_, e)) if !e.is_empty() && !e.contains('/') => e,
        _ => "srt",
    }
}

/// A URL with its token hidden, for logs.
fn redact(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(p) = rest.find(['?', '&']) {
        out.push_str(&rest[..=p]);
        rest = &rest[p + 1..];
        let end = rest.find('&').unwrap_or(rest.len());
        let (param, tail) = rest.split_at(end);
        match param.split_once('=') {
            Some((k, _))
                if k.eq_ignore_ascii_case("api_key") || k.eq_ignore_ascii_case("apikey") =>
            {
                out.push_str(k);
                out.push_str("=REDACTED");
            }
            _ => out.push_str(param),
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Background reporter for one playback: Start, Progress every 10 s, then Stopped +
/// resume position once mpv is gone. Then asks the UI to refresh the home screen.
fn spawn_reporter(
    client: Arc<Client>,
    item: Item,
    session: Option<String>,
    exited: Arc<std::sync::atomic::AtomicBool>,
    weak: slint::Weak<App>,
) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let start = item
            .user_data
            .as_ref()
            .map(|u| u.playback_position_ticks)
            .unwrap_or(0);
        let run = item.run_time_ticks.unwrap_or(0);
        let mut pos = start;
        let ses = session.as_deref();
        let _ = client.report("start", &item.id, pos, false, ses);
        let mut last = Instant::now();
        while !exited.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            if let Some(p) = mpv_get("time-pos").and_then(|v| v.as_f64()) {
                pos = (p * 1e7) as i64;
            }
            if last.elapsed() >= Duration::from_secs(10) {
                last = Instant::now();
                let paused = mpv_get("pause").and_then(|v| v.as_bool()).unwrap_or(false);
                if let Err(e) = client.report("progress", &item.id, pos, paused, ses) {
                    eprintln!("{e}");
                }
            }
        }
        let _ = client.report("stop", &item.id, pos, false, ses);
        if let Some(s) = ses {
            client.stop_transcode(s);
        }
        if let Err(e) = client.save_position(&item.id, pos, run) {
            eprintln!("{e}");
        }
        eprintln!("SAVED id={} pos={}", item.id, fmt_ticks(pos));
        // refresh in place: no partial set (it would drop rows for a moment)
        fetch_home(&client, false, |rows| {
            save_home(&client.auth.user_id, &rows);
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                with_state(|state| {
                    let mut st = state.borrow_mut();
                    install_screen(&app, &mut st, rows, false, Some(0));
                });
            });
        });
    });
}

/// Start mpv for a movie or an episode whose full item (media info, defaults) is known.
/// `poll` watches the process and cleans up when it exits.
pub(crate) fn start(ctx: &Ctx, poll: &Rc<slint::Timer>, it: Item) {
    let (weak, player, client, auth, t0) = (&ctx.app, &ctx.player, &ctx.client, &ctx.auth, ctx.t0);
    let lim = settings::effective(&settings::current(), settings::current_ssid().as_deref());
    let plan = plan(auth, &it, link_bps().unwrap_or(0), &lim);
    if let Some(a) = weak.upgrade() {
        a.set_starting_note(plan.note.clone().into());
    }
    let start = it
        .user_data
        .as_ref()
        .map(|u| u.playback_position_ticks as f64 / 1e7)
        .unwrap_or(0.0);
    let title = match (&it.series_name, it.parent_index_number, it.index_number) {
        (Some(s), Some(se), Some(ep)) => format!("{s} - S{se}E{ep} - {}", it.name),
        _ => it.name.clone(),
    };
    // what the phone remote shows as "now playing"
    let now = serde_json::json!({
        "id": it.id, "name": it.name, "series": it.series_name, "season": it.parent_index_number,
        "episode": it.index_number, "year": it.production_year, "runTicks": it.run_time_ticks.unwrap_or(0),
        "imageId": poster_id(&it, false),
        "names": { "audio": plan.audio_names, "sub": plan.sub_names },
    });
    let _ = std::fs::write(NOW_FILE, now.to_string());
    // display mode for this video, switched off the UI thread while mpv starts
    let video = it
        .media_sources
        .first()
        .and_then(|m| m.media_streams.iter().find(|s| s.kind == "Video"));
    let direct = plan.session.is_none();
    let video_h = video.and_then(|v| v.height).filter(|_| direct);
    let fps = video.and_then(|v| v.real_frame_rate.or(v.average_frame_rate));
    let hdr_hint = direct && lim.hdr && video.and_then(|v| v.video_range.as_deref()) == Some("HDR");
    let mode = std::cell::Cell::new(Some(modeswitch::switch_for(
        video_h,
        fps,
        lim.hdr,
        lim.max_height,
    )));
    match mpv_command(
        &plan.url,
        start,
        &title,
        &MpvOpts {
            aid: plan.aid.clone(),
            sid: plan.sid.clone(),
            hdr_hint,
            ..MpvOpts::from_settings()
        },
    )
    .spawn()
    {
        Ok(child) => {
            eprintln!("MPV_START id={} t={}ms", it.id, t0.elapsed().as_millis());
            *player.borrow_mut() = Some(child);
            let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
            spawn_reporter(
                client.clone(),
                it.clone(),
                plan.session.clone(),
                exited.clone(),
                weak.clone(),
            );
            let sub_titles = plan
                .sub_names
                .iter()
                .skip(plan.sub_names.len() - plan.subs.len())
                .cloned();
            spawn_sub_loader(
                plan.subs.iter().cloned().zip(sub_titles).collect(),
                plan.select_sub,
                exited.clone(),
            );
            let weak = weak.clone();
            let player = player.clone();
            let poll_w = Rc::downgrade(poll);
            poll.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(500),
                move || {
                    let done = match player.borrow_mut().as_mut() {
                        Some(c) => matches!(c.try_wait(), Ok(Some(_)) | Err(_)),
                        None => true,
                    };
                    // mpv's window is up and covers the app: stop the spinner
                    if let Some(a) = weak.upgrade().filter(|a| a.get_starting()) {
                        if done || mpv_get("vo-configured").and_then(|v| v.as_bool()) == Some(true)
                        {
                            eprintln!("MPV_VIDEO t={}ms", t0.elapsed().as_millis());
                            a.set_starting(false);
                        }
                    }
                    if done {
                        if let Some(t) = poll_w.upgrade() {
                            t.stop();
                        }
                        *player.borrow_mut() = None;
                        exited.store(true, std::sync::atomic::Ordering::Relaxed);
                        if let Some(h) = mode.take() {
                            modeswitch::restore(h);
                        }
                        let _ = std::fs::remove_file(NOW_FILE);
                        eprintln!("MPV_EXIT t={}ms", t0.elapsed().as_millis());
                        if let Some(a) = weak.upgrade() {
                            a.window().request_redraw();
                        }
                    }
                },
            );
        }
        Err(e) => {
            if let Some(h) = mode.take() {
                modeswitch::restore(h);
            }
            let _ = std::fs::remove_file(NOW_FILE);
            eprintln!("mpv spawn failed: {e}");
            if let Some(a) = weak.upgrade() {
                a.set_starting(false);
            }
        }
    }
}

/// Back while starting: kill mpv; the poll timer sees the exit and cleans up.
pub(crate) fn install_cancel(app: &App, player: &Rc<RefCell<Option<Child>>>) {
    let weak = app.as_weak();
    let player = player.clone();
    app.on_cancel_start(move || {
        if let Some(c) = player.borrow_mut().as_mut() {
            let _ = c.kill(); // the poll timer sees the exit and cleans up
        }
        if let Some(a) = weak.upgrade() {
            a.set_starting(false);
        }
        eprintln!("START_CANCELLED");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpv_gets_subtitle_scale_and_passthrough_only_when_set() {
        let args = |o: &MpvOpts| {
            mpv_command("http://x/v", 0.0, "t", o)
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let plain = args(&MpvOpts {
            sub_scale: 1.0,
            passthrough: false,
            aid: None,
            sid: None,
            hdr_hint: false,
        });
        assert!(
            !plain
                .iter()
                .any(|a| a.starts_with("--sub-scale") || a.starts_with("--audio-spdif")),
            "{plain:?}"
        );
        let set = args(&MpvOpts {
            sub_scale: 1.25,
            passthrough: true,
            aid: Some("2".into()),
            sid: Some("3".into()),
            hdr_hint: false,
        });
        for want in [
            "--sub-scale=1.25",
            "--audio-spdif=ac3,eac3,dts,dts-hd,truehd",
            "--aid=2",
            "--sid=3",
        ] {
            assert!(set.iter().any(|a| a == want), "{want} missing in {set:?}");
        }
    }

    #[test]
    fn subtitle_urls_lose_their_token_in_logs() {
        let u =
            "http://s:8096/Videos/x/y/Subtitles/2/0/Stream.srt?ApiKey=SECRET&a=1&api_key=SECRET2";
        let r = redact(u);
        assert!(!r.contains("SECRET"), "{r}");
        assert_eq!(r, "http://s:8096/Videos/x/y/Subtitles/2/0/Stream.srt?ApiKey=REDACTED&a=1&api_key=REDACTED");
        assert_eq!(redact("http://s/a?b=1"), "http://s/a?b=1");
        assert_eq!(sub_ext(u), "srt");
        assert_eq!(sub_ext("http://s/Stream.vtt?api_key=1.2"), "vtt");
        assert_eq!(sub_ext("http://s.lan/x"), "srt");
    }
}
