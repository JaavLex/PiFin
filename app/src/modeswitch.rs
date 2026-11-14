//! Display mode during playback: 4K only for a 4K video (with the "4K & HDR"
//! setting on), and the refresh rate matched to the video's frame rate. Menus stay
//! in the mode the session picked; the previous mode comes back when mpv exits.

use std::process::Command;
use std::thread::JoinHandle;

#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    pub current: bool,
}

impl Mode {
    pub fn arg(&self) -> String {
        format!("{}x{}@{}Hz", self.width, self.height, self.refresh)
    }
}

/// First enabled output of `wlr-randr --json`: its name and modes.
pub fn parse_outputs(json: &str) -> Option<(String, Vec<Mode>)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let out = v.as_array()?.iter().find(|o| o["enabled"].as_bool() == Some(true))?;
    let modes = out["modes"].as_array()?.iter().filter_map(|m| Some(Mode {
        width: m["width"].as_i64()? as i32,
        height: m["height"].as_i64()? as i32,
        refresh: m["refresh"].as_f64()?,
        current: m["current"].as_bool().unwrap_or(false),
    })).collect();
    Some((out["name"].as_str()?.to_string(), modes))
}

/// How far a refresh rate is from a whole multiple of the frame rate (relative).
fn rate_error(refresh: f64, fps: f64) -> f64 {
    let r = refresh / fps;
    let k = r.round().max(1.0);
    (r - k).abs() / k
}

/// The mode to play a video in, or None to stay in the current mode.
/// 2160p only when the video is taller than 1080, the 4K & HDR setting is on,
/// max_height allows 2160 and the TV offers it. Refresh: the offered rate that is
/// a whole multiple of the video's frame rate (23.976 → 23.976/24, 59.94 → 59.94/60).
pub fn choose(modes: &[Mode], video_h: Option<i32>, fps: Option<f64>, hdr: bool, max_height: i32) -> Option<Mode> {
    let cur = modes.iter().find(|m| m.current)?;
    let want_4k = video_h.is_some_and(|h| h > 1080) && hdr && max_height >= 2160;
    let (w, h) = match modes.iter().filter(|m| m.height == 2160).max_by_key(|m| (m.width == 3840, m.width)) {
        Some(m) if want_4k => (m.width, 2160),
        _ => (cur.width, cur.height),
    };
    let same: Vec<&Mode> = modes.iter().filter(|m| m.width == w && m.height == h).collect();
    let matched = fps.filter(|f| *f > 1.0).and_then(|f| {
        same.iter()
            .filter(|m| rate_error(m.refresh, f) < 0.0015)
            // exact match first, then the higher rate (50 over 25)
            .min_by(|a, b| rate_error(a.refresh, f).total_cmp(&rate_error(b.refresh, f)).then(b.refresh.total_cmp(&a.refresh)))
            .copied()
    });
    let pick = match matched {
        Some(m) => m,
        None if h == cur.height && w == cur.width => return None,
        // new size, no matching rate: closest to the current rate
        None => same.iter().min_by(|a, b| (a.refresh - cur.refresh).abs().total_cmp(&(b.refresh - cur.refresh).abs())).copied()?,
    };
    if pick.width == cur.width && pick.height == cur.height && (pick.refresh - cur.refresh).abs() < 0.001 {
        return None;
    }
    Some(pick.clone())
}

fn wlr_randr(args: &[&str]) -> Option<String> {
    let out = Command::new("timeout").arg("5").arg("wlr-randr").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Off the UI thread: switch the mode for this video. The handle yields
/// (output, previous mode) when a switch was made; failures just keep the mode.
pub fn switch_for(video_h: Option<i32>, fps: Option<f64>, hdr: bool, max_height: i32) -> JoinHandle<Option<(String, String)>> {
    std::thread::spawn(move || {
        let (out, modes) = parse_outputs(&wlr_randr(&["--json"])?)?;
        let prev = modes.iter().find(|m| m.current)?.arg();
        let Some(m) = choose(&modes, video_h, fps, hdr, max_height) else {
            eprintln!("MODE keep {prev} (video {video_h:?} {fps:?} fps)");
            return None;
        };
        eprintln!("MODE {out}: {prev} -> {} (video {video_h:?} {fps:?} fps)", m.arg());
        match wlr_randr(&["--output", &out, "--mode", &m.arg()]) {
            Some(_) => Some((out, prev)),
            None => {
                eprintln!("MODE switch failed, playing in {prev}");
                None
            }
        }
    })
}

/// Off the UI thread: wait for the switch, then put the previous mode back.
pub fn restore(h: JoinHandle<Option<(String, String)>>) {
    std::thread::spawn(move || {
        if let Ok(Some((out, prev))) = h.join() {
            let ok = wlr_randr(&["--output", &out, "--mode", &prev]).is_some();
            eprintln!("MODE restore {out} {prev} ok={ok}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(width: i32, height: i32, refresh: f64) -> Mode {
        Mode { width, height, refresh, current: false }
    }

    /// A 4K TV in 1080p60 (the menus' mode).
    fn tv() -> Vec<Mode> {
        let mut v = vec![
            m(3840, 2160, 60.0), m(3840, 2160, 59.94), m(3840, 2160, 50.0), m(3840, 2160, 30.0),
            m(3840, 2160, 25.0), m(3840, 2160, 24.0), m(3840, 2160, 23.976), m(4096, 2160, 24.0),
            m(1920, 1080, 60.0), m(1920, 1080, 59.94), m(1920, 1080, 50.0), m(1920, 1080, 24.0), m(1920, 1080, 23.976),
        ];
        v[8].current = true;
        v
    }

    fn arg(x: Option<Mode>) -> Option<String> {
        x.map(|m| m.arg())
    }

    #[test]
    fn four_k_only_for_a_4k_video_with_the_setting_and_cap() {
        assert_eq!(arg(choose(&tv(), Some(2160), Some(23.976), true, 2160)), Some("3840x2160@23.976Hz".into()));
        // toggle off, cap 1080, 1080p video: stay 1080, only the rate follows
        assert_eq!(arg(choose(&tv(), Some(2160), Some(23.976), false, 2160)), Some("1920x1080@23.976Hz".into()));
        assert_eq!(arg(choose(&tv(), Some(2160), Some(23.976), true, 1080)), Some("1920x1080@23.976Hz".into()));
        assert_eq!(arg(choose(&tv(), Some(1080), Some(23.976), true, 2160)), Some("1920x1080@23.976Hz".into()));
    }

    #[test]
    fn rate_follows_the_frame_rate() {
        assert_eq!(arg(choose(&tv(), Some(1080), Some(24.0), true, 2160)), Some("1920x1080@24Hz".into()));
        assert_eq!(arg(choose(&tv(), Some(1080), Some(25.0), true, 2160)), Some("1920x1080@50Hz".into()));
        assert_eq!(arg(choose(&tv(), Some(1080), Some(50.0), true, 2160)), Some("1920x1080@50Hz".into()));
        assert_eq!(arg(choose(&tv(), Some(1080), Some(59.94), true, 2160)), Some("1920x1080@59.94Hz".into()));
        assert_eq!(arg(choose(&tv(), Some(2160), Some(25.0), true, 2160)), Some("3840x2160@50Hz".into()));
        // already right, or unknown frame rate: no change
        assert_eq!(choose(&tv(), Some(1080), Some(60.0), true, 2160), None);
        assert_eq!(choose(&tv(), Some(1080), Some(30.0), true, 2160), None);
        assert_eq!(choose(&tv(), Some(1080), None, true, 2160), None);
    }

    #[test]
    fn modes_the_tv_does_not_offer_are_not_used() {
        let hd: Vec<Mode> = tv().into_iter().filter(|m| m.height == 1080 && m.refresh != 23.976 && m.refresh != 24.0).collect();
        assert_eq!(choose(&hd, Some(2160), Some(23.976), true, 2160), None);
        // 4K offered only at 30 Hz: 4K at the rate closest to the current one
        let mut v = vec![m(3840, 2160, 30.0), m(1920, 1080, 60.0)];
        v[1].current = true;
        assert_eq!(arg(choose(&v, Some(2160), Some(23.976), true, 2160)), Some("3840x2160@30Hz".into()));
    }

    #[test]
    fn parses_wlr_randr_json() {
        let j = r#"[{"name":"HDMI-A-1","enabled":true,"modes":[{"width":1920,"height":1080,"refresh":60.0,"preferred":false,"current":true}]}]"#;
        let (out, modes) = parse_outputs(j).unwrap();
        assert_eq!((out.as_str(), modes[0].arg(), modes[0].current), ("HDMI-A-1", "1920x1080@60Hz".to_string(), true));
    }
}
