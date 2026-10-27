//! The two programs that pull music off the web, kept inside the user's
//! `.babutela` folder so they are not shared between accounts: yt-dlp
//! (reads YouTube and SoundCloud) and QuickJS-NG (the JavaScript runtime
//! yt-dlp runs the challenge solver scripts on). Downloaded from GitHub
//! the first time music is asked for, and swapped for a newer yt-dlp when
//! GitHub has one; QuickJS stays pinned (its only job is to run the
//! scripts yt-dlp ships).
//!
//! What came down is checked against the hashes GitHub lists before it is
//! kept.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// How often the latest version of yt-dlp is looked up; the file is used
/// as it is in between.
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60 * 24);

/// The pinned QuickJS-NG binary (Windows x86_64, v0.17.0). It only runs the
/// EJS scripts yt-dlp ships, so there is no reason to chase newer releases:
/// a change in QuickJS is not what fixes YouTube.
const QJS_VERSION: &str = "v0.17.0";
const QJS_URL: &str =
    "https://github.com/quickjs-ng/quickjs/releases/download/v0.17.0/qjs-windows-x86_64.exe";
const QJS_SHA256: &str = "2aeabf0092c3262d6b2609824418f7dd7ed1f1df939f73b2b15645230cac0d77";

/// Where yt-dlp (Windows x86_64) is downloaded from. The release's
/// SHA2-256SUMS, in the same folder, says what each file is supposed to
/// hash to.
const YTDLP_LATEST: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

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

/// The ones already on disk and up to date, or downloaded now.
pub async fn ensure() -> Result<Tools, String> {
    let dir = home_dir()?.join(".babutela").join("yt-dlp");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("pasta de yt-dlp: {e}"))?;
    let _lock = gate().lock().await;

    let qjs = dir.join("qjs.exe");
    ensure_qjs(&qjs).await?;

    let yt_dlp = dir.join("yt-dlp.exe");
    let version_file = dir.join("yt-dlp.version");
    let stamp_file = dir.join("yt-dlp.checked");

    let have_version = tokio::fs::read_to_string(&version_file)
        .await
        .ok()
        .map(|s| s.trim().to_owned());
    let fresh = stamp_file
        .metadata()
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e < CHECK_EVERY).unwrap_or(false))
        .unwrap_or(false);

    if !(yt_dlp.is_file() && have_version.is_some() && fresh) {
        let latest = latest_ytdlp_version().await?;
        if !(yt_dlp.is_file() && have_version.as_deref() == Some(latest.as_str())) {
            download_ytdlp(&latest, &yt_dlp).await?;
            tokio::fs::write(&version_file, &latest)
                .await
                .map_err(|e| format!("gravar versão: {e}"))?;
        }
        let _ = tokio::fs::write(&stamp_file, "").await;
    }

    Ok(Tools { yt_dlp, qjs })
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

async fn latest_ytdlp_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("rede: {e}"))?;
    let body: serde_json::Value = client
        .get(YTDLP_LATEST)
        .send()
        .await
        .map_err(|e| format!("github: {e}"))?
        .json()
        .await
        .map_err(|e| format!("resposta do github: {e}"))?;
    body["tag_name"]
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| "github não devolveu a versão".to_owned())
}

async fn download_ytdlp(version: &str, out: &Path) -> Result<(), String> {
    let base = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{version}");
    let sums = format!("{base}/SHA2-256SUMS");
    let bin = format!("{base}/yt-dlp.exe");

    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("rede: {e}"))?;
    let text = client
        .get(&sums)
        .send()
        .await
        .map_err(|e| format!("github: {e}"))?
        .error_for_status()
        .map_err(|e| format!("github: {e}"))?
        .text()
        .await
        .map_err(|e| format!("github: {e}"))?;
    let expected = text
        .lines()
        .find_map(|line| {
            let mut cols = line.split_whitespace();
            let hash = cols.next()?;
            let name = cols.next()?;
            (name.trim_start_matches('*') == "yt-dlp.exe").then_some(hash.to_owned())
        })
        .ok_or_else(|| "SHA2-256SUMS sem yt-dlp.exe".to_owned())?;

    crate::logln!("música: baixando yt-dlp {version}");
    download_with_sha256(&bin, out, &expected).await?;
    Ok(())
}

/// Downloads to `path.tmp` and renames once the whole file hashes to
/// `expected`.
async fn download_with_sha256(url: &str, path: &Path, expected: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent())
        .timeout(Duration::from_secs(600))
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
    fn version_file_lives_next_to_the_binary() {
        let dir = PathBuf::from("/tmp/x");
        let bin = dir.join("yt-dlp.exe");
        let version = bin.with_file_name("yt-dlp.version");
        assert_eq!(version.parent(), Some(dir.as_path()));
    }
}
