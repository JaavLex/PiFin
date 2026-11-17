//! How to play an item: direct play or a server conversion, sized to the measured
//! link and the user's limits, and which audio and subtitle tracks to pick.

use crate::jellyfin::{self, Auth, Item};
use crate::settings;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Files above this bitrate are converted by the server instead of played directly:
/// the Pi's Wi-Fi can't keep up with Blu-ray remuxes (30+ Mbit/s).
const MAX_DIRECT_BITRATE: i64 = 16_000_000;

/// Converted streams: at most this bitrate (video + audio).
const MAX_TRANSCODE_BITRATE: i64 = 10_000_000;

/// Last measured throughput from the server (bit/s, 0 = unknown) and when. Measured
/// when Play is pressed (a minute's result is reused), so a weak hotel or home
/// Wi-Fi gets a stream it can carry instead of freezing every few seconds.
static LINK: Mutex<Option<(Instant, u64)>> = Mutex::new(None);

pub(crate) fn link_bps() -> Option<u64> {
    LINK.lock()
        .ok()?
        .filter(|(t, _)| t.elapsed() < Duration::from_secs(60))
        .map(|(_, b)| b)
}

/// Remember a throughput measurement for the next Play.
pub(crate) fn record_link(rate: u64) {
    if let Ok(mut l) = LINK.lock() {
        *l = Some((Instant::now(), rate));
    }
}

/// (largest file bitrate played directly, bitrate of a converted stream) for a link.
/// Direct play may use 80% of the link (it reads ahead in bursts); a converted stream
/// gets 60%, since Wi-Fi swings by half from one minute to the next.
fn bitrate_budget(link: u64) -> (i64, i64) {
    if link == 0 {
        return (MAX_DIRECT_BITRATE, MAX_TRANSCODE_BITRATE);
    }
    let l = link as f64;
    (
        ((l * 0.8) as i64).min(MAX_DIRECT_BITRATE),
        ((l * 0.6) as i64).clamp(1_500_000, MAX_TRANSCODE_BITRATE),
    )
}

/// How to play an item: the URL, extra subtitle files, and the track names the
/// phone remote shows (in mpv's order).
pub(crate) struct Plan {
    pub(crate) url: String,
    pub(crate) subs: Vec<String>,
    pub(crate) session: Option<String>,
    pub(crate) audio_names: Vec<String>,
    pub(crate) sub_names: Vec<String>,
    /// shown while starting when the connection forced a lower quality
    pub(crate) note: String,
    /// mpv's track numbers for the server's default tracks (None = leave mpv's choice)
    pub(crate) aid: Option<String>,
    pub(crate) sid: Option<String>,
    /// index into `subs` of the subtitle to switch on once it is added
    pub(crate) select_sub: Option<usize>,
}

#[derive(Debug, Default, PartialEq)]
struct Tracks {
    aid: Option<String>,
    sid: Option<String>,
    select_sub: Option<usize>,
}

/// The tracks the server's defaults ask for, in mpv's terms. Direct play: mpv numbers the
/// file's own tracks from 1 in file order (the order Jellyfin lists them); an external
/// text subtitle is added later and selected there. Converted: the stream carries one audio
/// track and every text subtitle is added as a file. A default that is not found leaves
/// mpv's own choice; no default subtitle means off.
fn tracks(
    streams: &[&jellyfin::MediaStream],
    defaults: Option<(Option<i32>, Option<i32>)>,
    convert: bool,
) -> Tracks {
    let Some((audio, sub)) = defaults else {
        return Tracks::default();
    };
    let mut t = Tracks::default();
    if !convert {
        let internal_audio = streams
            .iter()
            .filter(|s| s.kind == "Audio" && !s.is_external);
        t.aid = audio
            .and_then(|i| internal_audio.map(|s| s.index).position(|x| x == i))
            .map(|p| (p + 1).to_string());
    }
    match sub {
        None => t.sid = Some("no".into()),
        Some(i) => {
            let internal = streams
                .iter()
                .filter(|s| s.kind == "Subtitle" && !s.is_external);
            if let Some(p) = internal
                .map(|s| s.index)
                .position(|x| x == i)
                .filter(|_| !convert)
            {
                t.sid = Some((p + 1).to_string());
                return t;
            }
            let files = streams.iter().filter(|s| {
                s.kind == "Subtitle" && s.is_text_subtitle_stream && (convert || s.is_external)
            });
            t.select_sub = files.map(|s| s.index).position(|x| x == i);
        }
    }
    t
}

/// What to do with a file: play it as is, or have the server convert it.
#[derive(Debug, PartialEq)]
enum Decision {
    Direct,
    Convert { target: i64, max_height: i32 },
}

/// The link's budget, lowered by the user's limits (global or per network).
fn decide(
    source_bitrate: i64,
    source_height: Option<i32>,
    link: u64,
    lim: &settings::Playback,
) -> Decision {
    let (mut direct_max, mut target) = bitrate_budget(link);
    if lim.max_bitrate > 0 {
        direct_max = direct_max.min(lim.max_bitrate);
        target = target.min(lim.max_bitrate);
    }
    let target = target.clamp(1_500_000, MAX_TRANSCODE_BITRATE);
    if lim.prefer_original
        && source_bitrate <= direct_max
        && source_height.is_none_or(|h| h <= lim.max_height)
    {
        Decision::Direct
    } else {
        Decision::Convert {
            target,
            max_height: lim.max_height,
        }
    }
}

/// Widest picture for a height cap.
fn width_for_height(h: i32) -> i32 {
    match h {
        2160.. => 3840,
        1080..=2159 => 1920,
        720..=1079 => 1280,
        _ => 854,
    }
}

pub(crate) fn plan(auth: &Auth, it: &Item, link: u64, lim: &settings::Playback) -> Plan {
    let server = auth.server.trim_end_matches('/');
    let token = &auth.token;
    let ms = it.media_sources.first();
    let msid = ms.map(|m| m.id.clone()).unwrap_or_else(|| it.id.clone());
    let streams: Vec<&jellyfin::MediaStream> = ms
        .map(|m| m.media_streams.iter().collect())
        .unwrap_or_default();
    let name = |s: &jellyfin::MediaStream| {
        s.display_title
            .clone()
            .or(s.title.clone())
            .unwrap_or_else(|| format!("Track {}", s.index))
    };
    let sub_url = |s: &jellyfin::MediaStream| {
        format!(
            "{server}/Videos/{}/{msid}/Subtitles/{}/0/Stream.srt?ApiKey={token}",
            it.id, s.index
        )
    };
    let bitrate = ms.and_then(|m| m.bitrate).unwrap_or(0);
    let height = streams
        .iter()
        .find(|s| s.kind == "Video")
        .and_then(|s| s.height);
    let (link_direct_max, link_target) = bitrate_budget(link);
    let decision = decide(bitrate, height, link, lim);
    let tr = tracks(
        &streams,
        it.defaults,
        matches!(decision, Decision::Convert { .. }),
    );
    eprintln!("PLAN id={} source_bitrate={bitrate} height={height:?} link={link} direct_max={link_direct_max} target={link_target} limits={lim:?} -> {decision:?} defaults={:?} aid={:?} sid={:?} select_sub={:?}", it.id, it.defaults, tr.aid, tr.sid, tr.select_sub);
    let Decision::Convert { target, max_height } = decision else {
        // direct play: internal tracks in file order, then external text subs
        let audio_names = streams
            .iter()
            .filter(|s| s.kind == "Audio" && !s.is_external)
            .map(|s| name(s))
            .collect();
        let mut sub_names: Vec<String> = streams
            .iter()
            .filter(|s| s.kind == "Subtitle" && !s.is_external)
            .map(|s| name(s))
            .collect();
        let ext: Vec<&&jellyfin::MediaStream> = streams
            .iter()
            .filter(|s| s.kind == "Subtitle" && s.is_external && s.is_text_subtitle_stream)
            .collect();
        sub_names.extend(ext.iter().map(|s| name(s)));
        return Plan {
            url: format!(
                "{server}/Videos/{}/stream?static=true&mediaSourceId={msid}&api_key={token}",
                it.id
            ),
            subs: ext.iter().map(|s| sub_url(s)).collect(),
            session: None,
            audio_names,
            sub_names,
            note: String::new(),
            aid: tr.aid,
            sid: tr.sid,
            select_sub: tr.select_sub,
        };
    };
    // converted by the server (hardware encoder on the server side), HLS so seeking works
    let session = std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .unwrap_or_default()
        .trim()
        .replace('-', "");
    let wanted = it
        .defaults
        .and_then(|d| d.0)
        .and_then(|i| streams.iter().find(|s| s.kind == "Audio" && s.index == i));
    let audio = wanted
        .or(streams.iter().find(|s| s.kind == "Audio" && s.is_default))
        .or(streams.iter().find(|s| s.kind == "Audio"));
    let text_subs: Vec<&&jellyfin::MediaStream> = streams
        .iter()
        .filter(|s| s.kind == "Subtitle" && s.is_text_subtitle_stream)
        .collect();
    // a weak link gets a lighter stream: less audio, and 720p below ~3.5 Mbit/s
    // (sharper than a starved 1080p)
    let audio_br: i64 = if target < 6_000_000 { 256_000 } else { 384_000 };
    let video_br = (target - audio_br).max(1_200_000);
    let width = (if video_br < 3_500_000 { 1280 } else { 1920 }).min(width_for_height(max_height));
    eprintln!("TRANSCODE id={} video={video_br} audio={audio_br} width={width} max_height={max_height} subs={} burn_in={:?}", it.id, text_subs.len(), burn_in(&streams, it.defaults));
    // only the link is named: no note when just the user's limits caused the conversion
    let note = if bitrate > link_direct_max
        && (link_target < MAX_TRANSCODE_BITRATE || bitrate <= MAX_DIRECT_BITRATE)
    {
        format!(
            "lower quality for this connection (≈{} Mbit/s)",
            (link + 500_000) / 1_000_000
        )
    } else {
        String::new()
    };
    Plan {
        url: format!(
            "{server}/Videos/{}/master.m3u8?MediaSourceId={msid}&DeviceId={}&PlaySessionId={session}&ApiKey={token}\
             &VideoCodec=h264&AudioCodec=aac&VideoBitrate={video_br}&AudioBitrate={audio_br}&MaxAudioChannels=6\
             &TranscodingMaxAudioChannels=6&MaxWidth={width}&MaxHeight={max_height}&SegmentContainer=ts&MinSegments=1&BreakOnNonKeyFrames=True{}{}",
            it.id, auth.device_id,
            audio.map(|a| format!("&AudioStreamIndex={}", a.index)).unwrap_or_default(),
            burn_in(&streams, it.defaults)
        ),
        note,
        subs: text_subs.iter().map(|s| sub_url(s)).collect(),
        session: Some(session),
        audio_names: audio.map(|a| vec![format!("{} (converted)", name(a))]).unwrap_or_default(),
        sub_names: text_subs.iter().map(|s| name(s)).collect(),
        aid: None,
        sid: tr.sid,
        select_sub: tr.select_sub,
        }
}

/// Burn-in request for the default subtitle when it is image-based (PGS, DVD, DVB): those
/// cannot be added as text files, so the server draws them into the converted picture.
fn burn_in(
    streams: &[&jellyfin::MediaStream],
    defaults: Option<(Option<i32>, Option<i32>)>,
) -> String {
    let Some(i) = defaults.and_then(|d| d.1) else {
        return String::new();
    };
    match streams
        .iter()
        .find(|s| s.kind == "Subtitle" && s.index == i)
    {
        Some(s) if !s.is_text_subtitle_stream => {
            format!("&SubtitleStreamIndex={i}&SubtitleMethod=Encode")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jellyfin::MediaSource;

    fn item(bitrate: i64) -> Item {
        Item {
            id: "i1".into(),
            media_sources: vec![MediaSource {
                id: "m1".into(),
                bitrate: Some(bitrate),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn auth() -> Auth {
        Auth {
            server: "http://jf:8096".into(),
            user_id: "u".into(),
            token: "t".into(),
            device_id: "d".into(),
        }
    }

    fn none() -> settings::Playback {
        settings::Playback::default()
    }

    #[test]
    fn budget_follows_the_link() {
        assert_eq!(
            bitrate_budget(0),
            (MAX_DIRECT_BITRATE, MAX_TRANSCODE_BITRATE)
        ); // unknown: as before
        assert_eq!(bitrate_budget(7_400_000), (5_920_000, 4_440_000));
        assert_eq!(
            bitrate_budget(100_000_000),
            (MAX_DIRECT_BITRATE, MAX_TRANSCODE_BITRATE)
        );
        assert_eq!(bitrate_budget(1_000_000), (800_000, 1_500_000)); // floor for converted streams
    }

    #[test]
    fn weak_link_converts_to_a_stream_it_can_carry() {
        // a 30 Mbit/s file: a 10 Mbit/s conversion on a ~7 Mbit/s Wi-Fi froze every 10-20 s
        let p = plan(&auth(), &item(30_000_000), 7_400_000, &none());
        assert!(
            p.url.contains("master.m3u8")
                && p.url.contains("VideoBitrate=4184000")
                && p.url.contains("MaxWidth=1920"),
            "{}",
            p.url
        );
        assert!(!p.note.is_empty());
        // very weak: 720p
        let p = plan(&auth(), &item(30_000_000), 4_000_000, &none());
        assert!(
            p.url.contains("VideoBitrate=2144000") && p.url.contains("MaxWidth=1280"),
            "{}",
            p.url
        );
        // a light file still plays directly; one too heavy for this link is converted
        assert!(plan(&auth(), &item(5_000_000), 7_400_000, &none())
            .url
            .contains("static=true"));
        assert!(plan(&auth(), &item(7_000_000), 7_400_000, &none())
            .url
            .contains("master.m3u8"));
        // unknown link: the old rule (direct up to 16 Mbit/s, else 10 Mbit/s, no note)
        assert!(plan(&auth(), &item(12_000_000), 0, &none())
            .url
            .contains("static=true"));
        let p = plan(&auth(), &item(30_000_000), 0, &none());
        assert!(
            p.url.contains("VideoBitrate=9616000") && p.note.is_empty(),
            "{}",
            p.url
        );
    }

    #[test]
    fn decide_applies_the_users_limits() {
        let lim = |max_bitrate, max_height, prefer_original| settings::Playback {
            max_bitrate,
            max_height,
            hdr: false,
            prefer_original,
        };
        // a 6 Mbit/s cap on a fast link converts an 8 Mbit/s file at 6 Mbit/s
        assert_eq!(
            decide(8_000_000, None, 50_000_000, &lim(6_000_000, 1080, true)),
            Decision::Convert {
                target: 6_000_000,
                max_height: 1080
            }
        );
        // a 2160p file under a 1080 cap is converted, however light
        assert_eq!(
            decide(5_000_000, Some(2160), 0, &lim(0, 1080, true)),
            Decision::Convert {
                target: MAX_TRANSCODE_BITRATE,
                max_height: 1080
            }
        );
        assert_eq!(
            decide(5_000_000, Some(1080), 0, &lim(0, 1080, true)),
            Decision::Direct
        );
        // prefer original off: even a 2 Mbit/s file is converted
        assert!(matches!(
            decide(2_000_000, None, 0, &lim(0, 1080, false)),
            Decision::Convert { .. }
        ));
        // no limits: the link-only rules, as in weak_link_converts_to_a_stream_it_can_carry
        assert_eq!(
            decide(30_000_000, None, 7_400_000, &none()),
            Decision::Convert {
                target: 4_440_000,
                max_height: 1080
            }
        );
        assert_eq!(
            decide(5_000_000, None, 7_400_000, &none()),
            Decision::Direct
        );
        assert_eq!(
            decide(7_000_000, None, 7_400_000, &none()),
            Decision::Convert {
                target: 4_440_000,
                max_height: 1080
            }
        );
        assert_eq!(decide(12_000_000, None, 0, &none()), Decision::Direct);
    }

    #[test]
    fn plan_caps_the_converted_picture_to_max_height() {
        let lim = settings::Playback {
            max_height: 720,
            ..none()
        };
        let p = plan(&auth(), &item(30_000_000), 0, &lim);
        assert!(
            p.url.contains("MaxHeight=720&") && p.url.contains("MaxWidth=1280"),
            "{}",
            p.url
        );
        // only the user's cap caused this: no "lower quality for this connection" note
        assert!(p.note.is_empty());
        assert_eq!(
            [2160, 1080, 720, 480].map(width_for_height),
            [3840, 1920, 1280, 854]
        );
    }

    #[test]
    fn default_tracks_become_mpv_track_numbers() {
        let st = |index, kind: &str, ext: bool| jellyfin::MediaStream {
            index,
            kind: kind.into(),
            is_external: ext,
            is_text_subtitle_stream: kind == "Subtitle",
            ..Default::default()
        };
        // stream 0 video, 1-2 audio (eng, fre), 3-4 subtitles (eng, fre), 5 external subtitle
        let all = [
            st(0, "Video", false),
            st(1, "Audio", false),
            st(2, "Audio", false),
            st(3, "Subtitle", false),
            st(4, "Subtitle", false),
            st(5, "Subtitle", true),
        ];
        let s: Vec<&jellyfin::MediaStream> = all.iter().collect();
        let t = tracks(&s, Some((Some(2), Some(4))), false);
        assert_eq!(
            (t.aid.as_deref(), t.sid.as_deref(), t.select_sub),
            (Some("2"), Some("2"), None)
        );
        // an external default subtitle is selected when it is added
        let t = tracks(&s, Some((Some(1), Some(5))), false);
        assert_eq!(
            (t.aid.as_deref(), t.sid, t.select_sub),
            (Some("1"), None, Some(0))
        );
        // no default subtitle: off
        assert_eq!(
            tracks(&s, Some((Some(1), None)), false).sid.as_deref(),
            Some("no")
        );
        // not found, or not asked: leave mpv's choice
        assert_eq!(
            tracks(&s, Some((Some(-1), Some(-1))), false),
            Tracks::default()
        );
        assert_eq!(tracks(&s, None, false), Tracks::default());
        // converted: every text subtitle is a file, the default one is selected
        let t = tracks(&s, Some((Some(2), Some(4))), true);
        assert_eq!((t.aid, t.sid, t.select_sub), (None, None, Some(1)));
    }

    #[test]
    fn converted_stream_burns_in_only_an_image_default_subtitle() {
        let st = |index, text: bool| jellyfin::MediaStream {
            index,
            kind: "Subtitle".into(),
            is_text_subtitle_stream: text,
            ..Default::default()
        };
        // a file with two SRT subtitles (streams 2, 3) and two PGS subtitles (4, 5)
        let all = [st(2, true), st(3, true), st(4, false), st(5, false)];
        let s: Vec<&jellyfin::MediaStream> = all.iter().collect();
        assert_eq!(burn_in(&s, Some((Some(1), Some(2)))), "");
        assert_eq!(
            burn_in(&s, Some((Some(1), Some(5)))),
            "&SubtitleStreamIndex=5&SubtitleMethod=Encode"
        );
        assert_eq!(burn_in(&s, Some((Some(1), None))), "");
        assert_eq!(burn_in(&s, None), "");
        // converted: text subtitles become files numbered in order; the default is selected
        let t = tracks(&s, Some((Some(1), Some(3))), true);
        assert_eq!((t.sid, t.select_sub), (None, Some(1)));
        // an image default is burnt in, no text file selected
        assert_eq!(tracks(&s, Some((Some(1), Some(4))), true).select_sub, None);
    }
}
