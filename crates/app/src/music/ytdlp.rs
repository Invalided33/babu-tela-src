//! The two programs that pull music off the web, kept inside the user's
//! `.babutela` folder so they are not shared between accounts: yt-dlp
//! (reads YouTube and SoundCloud) and QuickJS-NG (a JavaScript runtime for
//! the challenge solver scripts yt-dlp ships, needed only when the fast
//! way in extractor.rs does not work). Downloaded from GitHub the first
//! time music is asked for, and yt-dlp swapped for a newer one when GitHub
//! has one; QuickJS stays pinned (its only job is to run the scripts
//! yt-dlp ships).
//!
//! yt-dlp comes as its folder build (`yt-dlp_win.zip`: the program with
//! its Python beside it), unpacked into a folder named after its version.
//! The single `yt-dlp.exe` unpacks that whole Python into %TEMP% every
//! time it starts, and Windows Defender scans what it unpacked each time:
//! measured here, 1.5 s to start against 0.6 s, for every song.
//!
//! What came down is checked against the hashes GitHub lists before it is
//! kept.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, PoisonError};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// How often the latest version of yt-dlp is looked up; the one here is
/// used as it is in between.
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60 * 24);

/// The pinned QuickJS-NG binary (Windows x86_64, v0.17.0). It only runs the
/// EJS scripts yt-dlp ships, so there is no reason to chase newer releases:
/// a change in QuickJS is not what fixes YouTube.
const QJS_VERSION: &str = "v0.17.0";
const QJS_URL: &str =
    "https://github.com/quickjs-ng/quickjs/releases/download/v0.17.0/qjs-windows-x86_64.exe";
const QJS_SHA256: &str = "2aeabf0092c3262d6b2609824418f7dd7ed1f1df939f73b2b15645230cac0d77";

/// Where the latest yt-dlp is looked up. The release's SHA2-256SUMS, next
/// to its files, says what each one is supposed to hash to.
const YTDLP_LATEST: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

/// The folder build of yt-dlp for Windows x86_64, as the releases name it.
const YTDLP_ZIP: &str = "yt-dlp_win.zip";

/// yt-dlp runs going on now (extractor.rs). The folder of an older version
/// is only removed while none is, since the program running may live in it.
pub static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// The user-agent GitHub expects on API calls (any one goes, as long as it
/// is set): the app's name and version.
fn user_agent() -> String {
    format!("babutela/{}", babutela_buildinfo::VERSION)
}

/// What the music module uses: the two executables.
#[derive(Clone, Debug)]
pub struct Tools {
    pub yt_dlp: PathBuf,
    pub qjs: PathBuf,
}

/// Only one job downloads at a time, however many music threads ask: a race
/// here could leave half-written files behind.
fn gate() -> &'static Mutex<()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

/// What `ensure` found the last time in this run of the app, and when. The
/// next songs use it as it is (no hashing QuickJS again, no reading the
/// version files) until it is time to look for a newer yt-dlp.
fn ready() -> &'static std::sync::Mutex<Option<(Tools, Instant)>> {
    static READY: OnceLock<std::sync::Mutex<Option<(Tools, Instant)>>> = OnceLock::new();
    READY.get_or_init(|| std::sync::Mutex::new(None))
}

fn ready_now() -> Option<Tools> {
    ready()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .filter(|(t, at)| at.elapsed() < CHECK_EVERY && t.yt_dlp.is_file() && t.qjs.is_file())
        .map(|(t, _)| t.clone())
}

/// The ones already on disk and up to date, or downloaded now.
pub async fn ensure() -> Result<Tools, String> {
    if let Some(tools) = ready_now() {
        return Ok(tools);
    }
    let dir = home_dir()?.join(".babutela").join("yt-dlp");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("pasta de yt-dlp: {e}"))?;
    let _lock = gate().lock().await;
    // Another song may have done all this while this one waited.
    if let Some(tools) = ready_now() {
        return Ok(tools);
    }
    let qjs = dir.join("qjs.exe");
    ensure_qjs(&qjs).await?;
    let yt_dlp = ensure_ytdlp(&dir).await?;
    let tools = Tools { yt_dlp, qjs };
    *ready().lock().unwrap_or_else(PoisonError::into_inner) = Some((tools.clone(), Instant::now()));
    Ok(tools)
}

/// Gets the programs ready ahead of the first song (the music window opens
/// with this), so the link pasted does not wait for them.
pub fn prewarm(rt: &tokio::runtime::Handle) {
    if ready_now().is_some() {
        return;
    }
    rt.spawn(async {
        if let Err(e) = ensure().await {
            crate::logln!("música: {e}");
        }
    });
}

/// `~/.babutela`'s parent (`USERPROFILE` on Windows).
fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| "pasta pessoal: não encontrada".to_owned())
}

async fn ensure_qjs(path: &Path) -> Result<(), String> {
    if path.is_file() {
        if sha256(path).await.ok().as_deref() == Some(QJS_SHA256) {
            return Ok(());
        }
        let _ = tokio::fs::remove_file(path).await;
    }
    crate::logln!("música: baixando QuickJS-NG {QJS_VERSION}");
    download_with_sha256(QJS_URL, path, QJS_SHA256).await?;
    Ok(())
}

/// yt-dlp.exe of the version in use, in `<dir>/<version>`: the one here
/// while it was looked up less than CHECK_EVERY ago (or GitHub cannot be
/// reached), else the latest, downloaded if it is not here yet.
async fn ensure_ytdlp(dir: &Path) -> Result<PathBuf, String> {
    let version_file = dir.join("yt-dlp.version");
    let stamp_file = dir.join("yt-dlp.checked");
    let have = tokio::fs::read_to_string(&version_file)
        .await
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|v| is_version(v));
    let installed = have
        .as_deref()
        .map(|v| program(dir, v))
        .filter(|p| p.is_file());
    let fresh = stamp_file
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e < CHECK_EVERY).unwrap_or(false))
        .unwrap_or(false);
    if let (Some(exe), Some(version), true) = (&installed, &have, fresh) {
        tidy(dir, version);
        return Ok(exe.clone());
    }
    let latest = match latest_ytdlp_version().await {
        Ok(v) => v,
        Err(e) => {
            // GitHub out of reach, or limiting the calls: the copy here
            // still plays.
            if let (Some(exe), Some(version)) = (installed, have) {
                crate::logln!(
                    "música: sem saber da versão nova do yt-dlp ({e}); segue a {version}"
                );
                return Ok(exe);
            }
            return Err(e);
        }
    };
    if !is_version(&latest) {
        return Err(format!(
            "o GitHub deu uma versão estranha do yt-dlp: {latest}"
        ));
    }
    let exe = program(dir, &latest);
    if !exe.is_file() {
        download_ytdlp(&latest, dir).await?;
    }
    if have.as_deref() != Some(latest.as_str()) {
        tokio::fs::write(&version_file, &latest)
            .await
            .map_err(|e| format!("gravar versão: {e}"))?;
    }
    let _ = tokio::fs::write(&stamp_file, "").await;
    tidy(dir, &latest);
    Ok(exe)
}

/// Where the program of `version` lives.
fn program(dir: &Path, version: &str) -> PathBuf {
    dir.join(version).join("yt-dlp.exe")
}

/// A release tag the way yt-dlp writes them ("2026.10.26", "2026.10.26.1").
/// A folder is named after it, so nothing else goes.
fn is_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !v.starts_with('.')
        && !v.ends_with('.')
        && !v.contains("..")
}

async fn latest_ytdlp_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("rede: {e}"))?;
    let body: serde_json::Value = client
        .get(YTDLP_LATEST)
        .send()
        .await
        .map_err(|e| format!("github: {e}"))?
        .error_for_status()
        .map_err(|e| format!("github: {e}"))?
        .json()
        .await
        .map_err(|e| format!("resposta do github: {e}"))?;
    body["tag_name"]
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| "github não devolveu a versão".to_owned())
}

/// Downloads the folder build of `version` and unpacks it into
/// `<dir>/<version>`.
async fn download_ytdlp(version: &str, dir: &Path) -> Result<(), String> {
    let base = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{version}");
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("rede: {e}"))?;
    let text = client
        .get(format!("{base}/SHA2-256SUMS"))
        .send()
        .await
        .map_err(|e| format!("github: {e}"))?
        .error_for_status()
        .map_err(|e| format!("github: {e}"))?
        .text()
        .await
        .map_err(|e| format!("github: {e}"))?;
    let expected =
        hash_for(&text, YTDLP_ZIP).ok_or_else(|| format!("SHA2-256SUMS sem o {YTDLP_ZIP}"))?;

    crate::logln!("música: baixando yt-dlp {version}");
    let zip = dir.join(format!("{version}.zip"));
    download_with_sha256(&format!("{base}/{YTDLP_ZIP}"), &zip, &expected).await?;
    let (dir, version) = (dir.to_owned(), version.to_owned());
    tokio::task::spawn_blocking(move || unpack(&zip, &dir, &version))
        .await
        .map_err(|e| format!("yt-dlp: {e}"))?
}

/// The hash SHA2-256SUMS lists for `name`.
fn hash_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut cols = line.split_whitespace();
        let hash = cols.next()?;
        let file = cols.next()?;
        (file.trim_start_matches('*') == name).then(|| hash.to_owned())
    })
}

/// Unpacks the release into `<dir>/<version>`, through a folder of its own
/// that takes the final name only once everything is in.
fn unpack(zip: &Path, dir: &Path, version: &str) -> Result<(), String> {
    let unpacking = dir.join(format!("{version}.unpacking"));
    let _ = std::fs::remove_dir_all(&unpacking);
    let unpacked = std::fs::File::open(zip)
        .map_err(|e| format!("abrir o yt-dlp: {e}"))
        .and_then(|file| {
            zip::ZipArchive::new(std::io::BufReader::new(file))
                .map_err(|e| format!("pacote do yt-dlp: {e}"))
        })
        .and_then(|mut archive| {
            // Paths are kept inside the folder (zip's `enclosed_name`).
            archive
                .extract(&unpacking)
                .map_err(|e| format!("desempacotar o yt-dlp: {e}"))
        });
    let _ = std::fs::remove_file(zip);
    if let Err(e) = unpacked {
        let _ = std::fs::remove_dir_all(&unpacking);
        return Err(e);
    }
    if !unpacking.join("yt-dlp.exe").is_file() {
        let _ = std::fs::remove_dir_all(&unpacking);
        return Err("o pacote do yt-dlp veio sem o yt-dlp.exe".into());
    }
    let done = dir.join(version);
    let _ = std::fs::remove_dir_all(&done);
    std::fs::rename(&unpacking, &done).map_err(|e| format!("mover o yt-dlp: {e}"))
}

/// Removes what older copies of yt-dlp left here: the folders of other
/// versions, half-unpacked ones, downloads that did not finish, and the
/// single-file yt-dlp.exe of before. Only while no yt-dlp runs (it may run
/// from a folder going away); otherwise a later time does it.
fn tidy(dir: &Path, keep: &str) {
    if RUNNING.load(Ordering::SeqCst) > 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            if name != keep && (is_version(&name) || name.ends_with(".unpacking")) {
                let _ = std::fs::remove_dir_all(&path);
            }
        } else if name == "yt-dlp.exe" || name.ends_with(".zip") || name.ends_with(".partial") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Downloads to `path.partial` and renames once the whole file hashes to
/// `expected`.
async fn download_with_sha256(url: &str, path: &Path, expected: &str) -> Result<(), String> {
    // A stall gives up, a slow line does not: the files are 2 to 24 MB.
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("rede: {e}"))?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("rede: {e}"))?
        .error_for_status()
        .map_err(|e| format!("rede: {e}"))?;

    let tmp = path.with_extension("partial");
    let _ = tokio::fs::remove_file(&tmp).await;
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("arquivo temporário: {e}"))?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("rede: {e}"))? {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("gravar: {e}"))?;
    }
    file.flush().await.map_err(|e| format!("gravar: {e}"))?;
    drop(file);

    let got = hex::encode(hasher.finalize());
    if got.eq_ignore_ascii_case(expected) {
        tokio::fs::rename(&tmp, path)
            .await
            .map_err(|e| format!("mover: {e}"))?;
        Ok(())
    } else {
        let _ = tokio::fs::remove_file(&tmp).await;
        Err(format!(
            "o SHA256 do arquivo baixado ({got}) não bate com o esperado ({expected})"
        ))
    }
}

async fn sha256(path: &Path) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("abrir: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| format!("ler: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_lock_lets_only_one_in() {
        let g = gate();
        let _held = g.lock().await;
        // A second lock attempt would wait, so just check we hold it.
        assert!(g.try_lock().is_err());
    }

    #[test]
    fn only_release_tags_name_a_folder() {
        assert!(is_version("2026.10.26"));
        assert!(is_version("2026.10.26.1"));
        for bad in [
            "",
            "..",
            "../x",
            "2026.10.26/..",
            "v2026",
            "2026..1",
            ".2026",
            "2026.",
        ] {
            assert!(!is_version(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_sums_give_the_zip_hash() {
        let sums = "aaa  yt-dlp.exe\nbbb  yt-dlp_win.zip\nccc *yt-dlp_x86.exe\n";
        assert_eq!(hash_for(sums, "yt-dlp_win.zip").as_deref(), Some("bbb"));
        assert_eq!(hash_for(sums, "yt-dlp_x86.exe").as_deref(), Some("ccc"));
        assert_eq!(hash_for(sums, "yt-dlp_win_arm64.zip"), None);
    }

    #[test]
    fn tidying_keeps_the_version_in_use_and_drops_the_rest() {
        let dir = std::env::temp_dir().join(format!("babutela-ytdlp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in [
            "2026.10.26",
            "2026.09.01",
            "2026.11.02.unpacking",
            "keep-me",
        ] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
            std::fs::write(dir.join(d).join("yt-dlp.exe"), b"x").unwrap();
        }
        for f in [
            "yt-dlp.exe",
            "qjs.exe",
            "2026.11.02.zip",
            "x.partial",
            "yt-dlp.version",
        ] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        tidy(&dir, "2026.10.26");
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["2026.10.26", "keep-me", "qjs.exe", "yt-dlp.version"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
