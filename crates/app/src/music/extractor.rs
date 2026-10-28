//! Reads the direct stream URL of a song off YouTube or SoundCloud,
//! through yt-dlp (the two together are what Discord's music bots used
//! before YouTube made bots sign in): the link goes in, a title, a length
//! and a URL that plays back for a while come out.
//!
//! Only links whose hostname matches the small list below are even tried,
//! so the yt-dlp process never gets to open an arbitrary address.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use super::ytdlp::Tools;

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
/// what came back. YouTube asking for a sign-in means the fallback through
/// the browsers' cookies gets tried, in [`BROWSERS`] order.
pub async fn extract(link: &str, tools: &Tools) -> Result<Extracted, String> {
    let service = classify(link).ok_or_else(|| crate::t!("music.only_links").to_owned())?;

    // First try without cookies: for most videos it works, and nobody's
    // account gets flagged for nothing.
    let out = run_ytdlp(tools, link, None).await?;
    let out = if looks_like_sign_in(&out) && service == Service::YouTube {
        // YouTube asked for a login. One of the browsers has an open
        // session that unlocks the download; the first that works wins.
        let original = out.stderr.clone();
        let mut winner: Option<Run> = None;
        for browser in BROWSERS {
            crate::logln!("música: tentando cookies do {browser}");
            match run_ytdlp(tools, link, Some(browser)).await {
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

    let v: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("resposta do yt-dlp: {e}"))?;
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
    })
}

/// What a single yt-dlp run came back with.
struct Run {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

/// Runs yt-dlp once with the args the music module always uses. When
/// `browser` is Some, `--cookies-from-browser` is passed too.
async fn run_ytdlp(tools: &Tools, link: &str, browser: Option<&str>) -> Result<Run, String> {
    let mut cmd = Command::new(&tools.yt_dlp);
    cmd.arg("--dump-single-json")
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("--no-call-home")
        // Don't hang forever on a slow TCP read, and try again a few
        // times on a transient failure (YouTube may 429 under load, or
        // a connection may be reset mid-handshake).
        .arg("--socket-timeout")
        .arg("15")
        .arg("--retries")
        .arg("3")
        .arg("--extractor-retries")
        .arg("3")
        // YouTube's defaults (`tv_samsung`, `visionos`, `web`) are picked
        // by yt-dlp to avoid the PO Token requirement, and the list moves
        // as YouTube keeps changing: don't override `player_client` here,
        // let yt-dlp pick. `formats=missing_pot` still considers audio
        // formats that normally need a PO Token: the common itag 251
        // (Opus 160 kb/s) often plays without one, and if it 403s the
        // fallback picks the next format automatically.
        .arg("--extractor-args")
        .arg("youtube:formats=missing_pot")
        .arg("--format")
        // Opus/Vorbis first for YouTube (160 kb/s itag 251), then any
        // audio-only, then best of anything with sound. SoundCloud picks
        // one of its own.
        .arg("bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio/best")
        .arg("--js-runtimes")
        .arg(format!("quickjs:{}", qjs_arg(&tools.qjs)));
    if let Some(name) = browser {
        cmd.arg("--cookies-from-browser").arg(name);
    }
    cmd.arg(link);
    cmd.kill_on_drop(true);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // No console window: the app runs in `windows_subsystem = "windows"`,
    // so otherwise Windows gives yt-dlp a console window of its own that
    // flashes on screen.
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

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
