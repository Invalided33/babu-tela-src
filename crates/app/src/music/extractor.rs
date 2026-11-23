//! Reads the direct stream URL of a song off YouTube or SoundCloud,
//! through yt-dlp (the two together are what Discord's music bots used
//! before YouTube made bots sign in): the link goes in, a title, a length
//! and a URL that plays back for a while come out.
//!
//! Only links whose hostname matches the small list below are even tried,
//! so the yt-dlp process never gets to open an arbitrary address.
//!
//! YouTube is read the fast way first: as YouTube's visionOS app, whose
//! audio needs no JavaScript challenge solved and no ad waited out. yt-dlp's
//! own choice of apps, tried next, takes about 12 s here (7 of them solving
//! the challenge in QuickJS, and the audio then only plays after the ad
//! YouTube says the TV app would show); the fast way, 1.5 to 2.5 s. The
//! visionOS app is one yt-dlp itself asks by default today; should YouTube
//! close it, the slow way still works.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use super::ytdlp::{RUNNING, Tools};

/// The hostnames links may name.
const HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
    "soundcloud.com",
    "www.soundcloud.com",
    "m.soundcloud.com",
    "on.soundcloud.com",
];

/// yt-dlp sometimes takes a while on YouTube.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);

/// The fast way in. `skip=hls` leaves out the HLS playlists, which only a
/// live stream needs (one then takes the slow way): reading them was half
/// a second of every song.
const FAST_ARGS: &str = "youtube:player_client=visionos;skip=hls";

/// yt-dlp's own choice of YouTube apps. `formats=missing_pot` still
/// considers audio formats that normally need a PO Token: the common itag
/// 251 (Opus 160 kb/s) often plays without one, and if it 403s the
/// fallback picks the next format automatically. YouTube's defaults are
/// picked by yt-dlp to avoid the PO Token requirement, and the list moves
/// as YouTube keeps changing: `player_client` is not overridden here.
const FULL_ARGS: &str = "youtube:formats=missing_pot";

/// The audio asked for: Opus/Vorbis first for YouTube (160 kb/s itag 251),
/// then any audio-only, then best of anything with sound. SoundCloud picks
/// one of its own.
const FORMAT: &str = "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio/best";

/// Browsers whose cookies yt-dlp can read. Each one is tried in turn when
/// YouTube asks for a sign-in: the first that has a working YouTube
/// session unlocks the download. The name is what `yt-dlp
/// --cookies-from-browser` takes.
const BROWSERS: &[&str] = &["chrome", "edge", "firefox", "brave", "opera", "vivaldi"];

/// `CREATE_NO_WINDOW` (`winbase.h`): yt-dlp is a console program, and
/// without it Windows gives it its own console window every time it is
/// spawned by a program compiled with `windows_subsystem = "windows"`.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The service the link belongs to (as the user sees it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    YouTube,
    SoundCloud,
}

impl Service {
    pub fn pretty(self) -> &'static str {
        match self {
            Service::YouTube => "YouTube",
            Service::SoundCloud => "SoundCloud",
        }
    }
}

/// What the DJ's app plays from.
#[derive(Clone, Debug)]
pub struct Extracted {
    /// The service as a label ("YouTube", "SoundCloud").
    pub service: Service,
    /// The video's or track's title.
    pub title: String,
    /// Who uploaded or posted it.
    pub uploader: Option<String>,
    /// Length in seconds (none for livestreams).
    pub duration: Option<u32>,
    /// The playable URL (a progressive file or an HLS playlist).
    pub stream_url: String,
    /// The file extension of the chosen format ("m4a", "webm", "mp3"...),
    /// which the decoder uses to pick a probe.
    pub ext: String,
    /// true when the chosen format is an HLS playlist (SoundCloud now,
    /// YouTube's livestreams).
    pub hls: bool,
    /// From when (seconds since 1970) YouTube serves the audio: until then
    /// it answers 403, as the ad it would have shown first has not ended.
    pub available_at: Option<f64>,
    /// Read the fast way: if its audio does not come, the slow way may.
    pub fast: bool,
}

/// How yt-dlp is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    Fast,
    Full,
}

/// Says whether `link` is one of the services we read from, and gives the
/// service back if so.
pub fn classify(link: &str) -> Option<Service> {
    let url = reqwest::Url::parse(link.trim()).ok()?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !HOSTS.contains(&host.as_str()) {
        return None;
    }
    if host.contains("soundcloud") {
        Some(Service::SoundCloud)
    } else {
        Some(Service::YouTube)
    }
}

/// Runs yt-dlp in `-J` (one JSON out) and picks the audio format out of
/// what came back: the fast way first for YouTube, then [`extract_full`].
pub async fn extract(link: &str, tools: &Tools) -> Result<Extracted, String> {
    let service = classify(link).ok_or_else(|| crate::t!("music.only_links").to_owned())?;
    if service == Service::YouTube {
        match run_ytdlp(tools, link, Way::Fast, None).await {
            Ok(r) if r.ok => match parse(service, &r.stdout, true) {
                Ok(found) => return Ok(found),
                Err(e) => crate::logln!("música: o caminho rápido não serviu ({e})"),
            },
            Ok(r) => crate::logln!(
                "música: o caminho rápido não serviu ({})",
                trimmed_error(&r.stderr)
            ),
            Err(e) => crate::logln!("música: o caminho rápido não serviu ({e})"),
        }
    }
    extract_full(link, tools).await
}

/// yt-dlp's own way. YouTube asking for a sign-in means the fallback
/// through the browsers' cookies gets tried, in [`BROWSERS`] order.
pub async fn extract_full(link: &str, tools: &Tools) -> Result<Extracted, String> {
    let service = classify(link).ok_or_else(|| crate::t!("music.only_links").to_owned())?;

    // First try without cookies: for most videos it works, and nobody's
    // account gets flagged for nothing.
    let out = run_ytdlp(tools, link, Way::Full, None).await?;
    let out = if looks_like_sign_in(&out) && service == Service::YouTube {
        // YouTube asked for a login. One of the browsers has an open
        // session that unlocks the download; the first that works wins.
        let original = out.stderr.clone();
        let mut winner: Option<Run> = None;
        for browser in BROWSERS {
            crate::logln!("música: tentando cookies do {browser}");
            match run_ytdlp(tools, link, Way::Full, Some(browser)).await {
                // Took: the stream URL is in the stdout.
                Ok(r) if r.ok => {
                    winner = Some(r);
                    break;
                }
                // yt-dlp ran but still refused, or the browser is not
                // there: try the next one.
                Ok(_) | Err(_) => continue,
            }
        }
        match winner {
            Some(r) => r,
            // Nothing worked. The original YouTube error is clearer than
            // the last browser's "could not find chrome": show that one.
            None => {
                return Err(crate::tf!(
                    "music.youtube_login",
                    detail = trimmed_error(&original)
                ));
            }
        }
    } else {
        out
    };
    if !out.ok {
        return Err(trimmed_error(&out.stderr));
    }
    parse(service, &out.stdout, false)
}

/// The song in yt-dlp's JSON.
fn parse(service: Service, json: &[u8], fast: bool) -> Result<Extracted, String> {
    let v: Value = serde_json::from_slice(json).map_err(|e| format!("resposta do yt-dlp: {e}"))?;
    let title = v["title"]
        .as_str()
        .unwrap_or("desconhecido")
        .trim()
        .to_owned();
    let uploader = v["uploader"]
        .as_str()
        .or_else(|| v["channel"].as_str())
        .map(|s| s.trim().to_owned());
    let duration = v["duration"]
        .as_f64()
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| d as u32);
    let stream_url = v["url"]
        .as_str()
        .ok_or_else(|| "yt-dlp não devolveu URL".to_owned())?
        .to_owned();
    let ext = v["ext"].as_str().unwrap_or("").to_ascii_lowercase();
    let protocol = v["protocol"].as_str().unwrap_or("").to_ascii_lowercase();
    let hls = protocol.contains("m3u8") || protocol.contains("hls") || stream_url.contains(".m3u8");
    let available_at = v["available_at"].as_f64().filter(|t| t.is_finite());

    if stream_url.is_empty() {
        return Err("yt-dlp devolveu URL vazia".to_owned());
    }

    Ok(Extracted {
        service,
        title,
        uploader,
        duration,
        stream_url,
        ext,
        hls,
        available_at,
        fast,
    })
}

/// What a single yt-dlp run came back with.
struct Run {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

/// Counts a yt-dlp run as going on for as long as it lives (ytdlp.rs
/// leaves the program's folder alone meanwhile).
struct Running;

impl Running {
    fn start() -> Running {
        RUNNING.fetch_add(1, Ordering::SeqCst);
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The arguments of one run, the link last.
fn args(tools: &Tools, link: &str, way: Way, browser: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = [
        "--dump-single-json",
        "--no-warnings",
        "--no-playlist",
        // Don't hang forever on a slow TCP read, and try again a few
        // times on a transient failure (YouTube may 429 under load, or a
        // connection may be reset mid-handshake).
        "--socket-timeout",
        "15",
        "--retries",
        "3",
        "--extractor-retries",
        "3",
        "--format",
        FORMAT,
        "--extractor-args",
    ]
    .map(str::to_owned)
    .into();
    match way {
        Way::Fast => a.push(FAST_ARGS.to_owned()),
        Way::Full => {
            a.push(FULL_ARGS.to_owned());
            // The challenge solver runs in QuickJS (here), or in Node.js
            // when this PC has a recent one (Deno, yt-dlp's default, too):
            // both are far faster at it (1 s against 7).
            a.push("--js-runtimes".to_owned());
            a.push(format!("quickjs:{}", qjs_arg(&tools.qjs)));
            a.push("--js-runtimes".to_owned());
            a.push("node".to_owned());
        }
    }
    if let Some(name) = browser {
        a.push("--cookies-from-browser".to_owned());
        a.push(name.to_owned());
    }
    a.push(link.to_owned());
    a
}

/// Runs yt-dlp once with the args the music module always uses. When
/// `browser` is Some, `--cookies-from-browser` is passed too.
async fn run_ytdlp(
    tools: &Tools,
    link: &str,
    way: Way,
    browser: Option<&str>,
) -> Result<Run, String> {
    let mut cmd = Command::new(&tools.yt_dlp);
    cmd.args(args(tools, link, way, browser));
    cmd.kill_on_drop(true);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // No console window: the app runs in `windows_subsystem = "windows"`,
    // so otherwise Windows gives yt-dlp a console window of its own that
    // flashes on screen.
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let _running = Running::start();
    let child = cmd.spawn().map_err(|e| format!("yt-dlp: {e}"))?;
    let out = tokio::time::timeout(EXTRACT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "yt-dlp demorou demais".to_owned())?
        .map_err(|e| format!("yt-dlp: {e}"))?;
    Ok(Run {
        ok: out.status.success(),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// true when the yt-dlp run failed with YouTube's sign-in prompt: the
/// extractor retries with the browser cookies.
fn looks_like_sign_in(r: &Run) -> bool {
    !r.ok
        && (r.stderr.contains("Sign in to confirm")
            || r.stderr.contains("sign in to confirm")
            || r.stderr.contains("Sign in to prove"))
}

/// A path for yt-dlp's `--js-runtimes quickjs:<path>` argument. On
/// Windows, the path may have spaces, so it is passed as-is (yt-dlp
/// splits on the first `:` only).
fn qjs_arg(qjs: &Path) -> String {
    qjs.to_string_lossy().into_owned()
}

/// The short form of a yt-dlp error, with the "ERROR:" line only.
fn trimmed_error(msg: &str) -> String {
    for line in msg.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ERROR:") {
            return rest.trim().to_owned();
        }
    }
    msg.lines()
        .next()
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "yt-dlp falhou sem explicar".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_hosts() {
        assert_eq!(
            classify("https://www.youtube.com/watch?v=abc"),
            Some(Service::YouTube)
        );
        assert_eq!(classify("https://youtu.be/abc"), Some(Service::YouTube));
        assert_eq!(
            classify("https://soundcloud.com/user/track"),
            Some(Service::SoundCloud)
        );
        assert_eq!(classify("https://example.com/x"), None);
        assert_eq!(classify("not a url"), None);
        assert_eq!(classify("file:///c:/x"), None);
    }

    #[test]
    fn trims_errors_to_the_message() {
        let s = "WARNING: foo\nERROR: Unable to extract\nmore stuff";
        assert_eq!(trimmed_error(s), "Unable to extract");
        let s2 = "yt-dlp exploded";
        assert_eq!(trimmed_error(s2), "yt-dlp exploded");
    }

    fn tools() -> Tools {
        Tools {
            yt_dlp: "C:\\y\\yt-dlp.exe".into(),
            qjs: "C:\\Program Files\\q\\qjs.exe".into(),
        }
    }

    #[test]
    fn the_fast_way_asks_the_visionos_app_and_needs_no_javascript() {
        let a = args(&tools(), "https://youtu.be/x", Way::Fast, None);
        let pos = a.iter().position(|s| s == "--extractor-args").unwrap();
        assert_eq!(a[pos + 1], "youtube:player_client=visionos;skip=hls");
        assert!(!a.iter().any(|s| s == "--js-runtimes"));
        assert!(!a.iter().any(|s| s == "--no-call-home"), "deprecated");
        assert_eq!(a.last().map(String::as_str), Some("https://youtu.be/x"));
    }

    #[test]
    fn the_full_way_keeps_yt_dlps_apps_and_offers_the_runtimes() {
        let a = args(&tools(), "https://youtu.be/x", Way::Full, Some("edge"));
        let pos = a.iter().position(|s| s == "--extractor-args").unwrap();
        assert_eq!(a[pos + 1], "youtube:formats=missing_pot");
        let runtimes: Vec<&str> = a
            .windows(2)
            .filter(|w| w[0] == "--js-runtimes")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(runtimes, ["quickjs:C:\\Program Files\\q\\qjs.exe", "node"]);
        let cookies = a
            .iter()
            .position(|s| s == "--cookies-from-browser")
            .unwrap();
        assert_eq!(a[cookies + 1], "edge");
        assert_eq!(a.last().map(String::as_str), Some("https://youtu.be/x"));
    }

    #[test]
    fn reads_the_song_and_when_its_audio_plays() {
        let json = br#"{"title":" Despacito ","channel":"Luis Fonsi","duration":282.0,
            "url":"https://rr2---sn-x.googlevideo.com/videoplayback?x=1","ext":"webm",
            "protocol":"https","available_at":1794317840}"#;
        let e = parse(Service::YouTube, json, true).unwrap();
        assert_eq!(e.title, "Despacito");
        assert_eq!(e.uploader.as_deref(), Some("Luis Fonsi"));
        assert_eq!(e.duration, Some(282));
        assert_eq!(e.ext, "webm");
        assert!(!e.hls && e.fast);
        assert_eq!(e.available_at, Some(1_794_317_840.0));
        let live =
            br#"{"title":"x","url":"https://m.example/index.m3u8","protocol":"m3u8_native"}"#;
        let e = parse(Service::YouTube, live, false).unwrap();
        assert!(e.hls && !e.fast);
        assert_eq!((e.available_at, e.duration), (None, None));
        assert!(parse(Service::YouTube, br#"{"title":"x"}"#, true).is_err());
    }

    #[test]
    fn sign_in_prompt_is_recognised() {
        let r = Run {
            ok: false,
            stdout: Vec::new(),
            stderr: "ERROR: [youtube] abc: Sign in to confirm you're not a bot. Use --cookies-from-browser".into(),
        };
        assert!(looks_like_sign_in(&r));
    }

    #[test]
    fn other_failures_are_not_mistaken_for_sign_in() {
        let r = Run {
            ok: false,
            stdout: Vec::new(),
            stderr: "ERROR: [youtube] abc: Video unavailable".into(),
        };
        assert!(!looks_like_sign_in(&r));
    }

    #[test]
    fn a_successful_run_is_not_a_sign_in() {
        let r = Run {
            ok: true,
            stdout: b"{}".to_vec(),
            stderr: String::new(),
        };
        assert!(!looks_like_sign_in(&r));
    }
}
