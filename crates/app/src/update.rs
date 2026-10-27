//! Updates, the way Discord's desktop app does them: every time the app
//! starts it looks for a newer version before showing anything, installs it
//! and opens again on it (Discord's splash screen, "Checking for updates";
//! its updater, as OpenAsar reproduces it, downloads and installs the new
//! version and then restarts into it), and while it runs a newer version is
//! downloaded in the background, ready for the next start.
//!
//! - The server publishes updates/latest.json next to the installer
//!   (`cargo xtask publicar` writes it):
//!   `{"version": "0.5.1", "file": "Babu-Tela-Setup.exe", "sha256": "...", "sigs": [{"k", "s"}]}`.
//!   `sigs` are the update keys' (`cargo xtask chave` on each signer's PC),
//!   which never go to Cloudflare: without enough of them a latest.json is
//!   refused ([`signed`]); and the version must be in the public record
//!   (e2e/record.rs), so an update made for one person alone would show.
//! - A newer installer is downloaded into [`dir`], checked against that
//!   hash and written down there ([`Pending`]).
//! - At the next start, before the window opens (main.rs), the installer is
//!   checked again and run with `--update` (babutela_buildinfo::Update): it
//!   waits for this app to quit, replaces it and opens it again.
//! - At startup Core asks the server first (its update gate): a newer
//!   version is downloaded with its progress on the screen and installed
//!   at once. If the server does not answer in a few seconds, the app opens
//!   on the version it has.
//! - Unlike Discord's Squirrel install, the app stays in the same folder
//!   with the same name: versioned folders lose taskbar pins and firewall
//!   rules, and the notifications' activator and "Iniciar com o Windows"
//!   point at the file.
//! - The same update is tried by at most [`MAX_TRIES`] starts, so one that
//!   fails never keeps the app from opening (Discord's splash retries
//!   forever); after that the banner's button is left.

use std::path::{Path, PathBuf};
use std::time::Duration;

use babutela_buildinfo::{Update, VERSION};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::net::Progress;
use crate::shell::Shell;

#[derive(Deserialize)]
struct LatestInfo {
    #[serde(default)]
    version: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    sha256: String,
    /// The signatures of the update keys over the three ([`signed`]).
    #[serde(default)]
    sigs: Vec<Sig>,
}

/// One signer's signature: their public key and the signature, in hex.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sig {
    pub k: String,
    pub s: String,
}

/// The public halves of the keys that sign updates (`cargo xtask chave`
/// on each signer's PC, crates/xtask/src/signing.rs), in hex and separated
/// by commas, and how many of them must sign: put in by `cargo xtask
/// release` from `assinantes.txt`. A build without them (one for testing)
/// takes no update.
const UPDATE_KEYS: Option<&str> = option_env!("BABUTELA_CHAVES_PUBLICAS");
const UPDATE_MINIMUM: Option<&str> = option_env!("BABUTELA_ASSINATURAS_MINIMAS");

/// Whether enough of the update keys signed this version, file and hash
/// (the same string as signing.rs), each counted once. Whoever holds the
/// Cloudflare account can publish an installer, but not these signatures:
/// the keys never go there; and with several signers no single one of
/// them, the one who publishes included, can sign an update alone.
fn signed(version: &str, file: &str, sha256: &str, sigs: &[Sig]) -> bool {
    let keys: Vec<Vec<u8>> = UPDATE_KEYS
        .unwrap_or_default()
        .split(',')
        .filter_map(|k| hex::decode(k.trim()).ok())
        .filter(|k| k.len() == 32)
        .collect();
    let minimum = UPDATE_MINIMUM
        .and_then(|m| m.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    if keys.len() < minimum {
        return false;
    }
    let message = format!(
        "babutela/atualizacao/v1\n{version}\n{file}\n{}",
        sha256.to_ascii_lowercase()
    );
    let mut counted: Vec<&Vec<u8>> = Vec::new();
    for sig in sigs {
        let (Ok(k), Ok(s)) = (hex::decode(sig.k.trim()), hex::decode(sig.s.trim())) else {
            continue;
        };
        let Some(key) = keys.iter().find(|known| **known == k) else {
            continue;
        };
        if counted.contains(&key) {
            continue;
        }
        if ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
            .verify(message.as_bytes(), &s)
            .is_ok()
        {
            counted.push(key);
        }
    }
    counted.len() >= minimum
}

/// A downloaded installer, checked against latest.json, waiting in [`dir`]
/// for the next start (`atualizacao.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub version: String,
    /// Its name in the folder.
    pub file: String,
    pub sha256: String,
    /// The server it came from: an app pointed at another one leaves it.
    pub from: String,
    /// How many starts have run it so far.
    #[serde(default)]
    pub tries: u32,
    /// Its name in latest.json and the update keys' signatures, checked
    /// again before it runs.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub sigs: Vec<Sig>,
}

/// The record of the update waiting in the folder.
const RECORD: &str = "atualizacao.json";

/// Start of the name a downloaded installer gets.
const INSTALLER_PREFIX: &str = "babutela-update-";

/// How many starts try the same update before leaving it to the banner's
/// button.
pub const MAX_TRIES: u32 = 2;

/// How often the running app looks for a new version.
pub const CHECK_EVERY: Duration = Duration::from_secs(4 * 3600);

pub enum Outcome {
    /// An installer for this version is downloaded and checked; `tries`
    /// starts have run it.
    Ready { version: String, tries: u32 },
    /// Nothing newer.
    None,
    /// Another check is downloading right now.
    Busy,
}

/// Whether version `a` is newer than `b` (major.minor.patch).
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> [i64; 3] {
        let mut out = [0; 3];
        for (slot, p) in out.iter_mut().zip(v.split('.')) {
            *slot = p.parse().unwrap_or(0);
        }
        out
    };
    parts(a) > parts(b)
}

/// Escapes a file name for a URL path, like Go's url.PathEscape.
fn path_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~$&+,:;=@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------- the folder

/// Where downloaded updates wait: %LOCALAPPDATA%\Prosa\updates (big, and
/// for this PC only, so not in the roaming profile); a test profile
/// (PROSA_USERDATA) has its own.
pub fn dir(shell: &Shell) -> PathBuf {
    if shell.own_profile()
        && let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(local).join("Prosa").join("updates");
    }
    shell.path("updates")
}

/// An installer's name in the folder: only what this module itself makes.
fn installer_name(version: &str) -> String {
    // The version comes from the server: only digits and dots.
    let tag: String = version
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    format!("{INSTALLER_PREFIX}{tag}.exe")
}

/// The version in an installer's name, if it is one of ours.
fn version_of(name: &str) -> Option<&str> {
    let version = name.strip_prefix(INSTALLER_PREFIX)?.strip_suffix(".exe")?;
    (!version.is_empty() && version.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
        .then_some(version)
}

/// The update waiting in `dir`, if it is for a newer version than this
/// app's and its file is there.
pub fn pending(dir: &Path) -> Option<Pending> {
    let bytes = std::fs::read(dir.join(RECORD)).ok()?;
    let p: Pending = serde_json::from_slice(&bytes).ok()?;
    (version_of(&p.file).is_some() && newer(&p.version, VERSION) && dir.join(&p.file).is_file())
        .then_some(p)
}

/// Writes the record down, replacing the one there in a single step.
fn save(dir: &Path, p: &Pending) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let fresh = dir.join(format!("{RECORD}.new"));
    std::fs::write(&fresh, serde_json::to_vec_pretty(p)?)?;
    std::fs::rename(&fresh, dir.join(RECORD))
}

/// Forgets an update: its record and its installer.
fn discard(dir: &Path, p: &Pending) {
    let _ = std::fs::remove_file(dir.join(RECORD));
    let _ = std::fs::remove_file(dir.join(&p.file));
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Whether the installer is still the file that was checked.
fn intact(dir: &Path, p: &Pending) -> bool {
    sha256_file(&dir.join(&p.file)).is_ok_and(|h| h.eq_ignore_ascii_case(&p.sha256))
}

/// Throws away what this version no longer needs: an update for it or an
/// older one, installers nothing points at, half downloads, and the
/// installers versions before 0.11.2 left in %TEMP%. The installer that just
/// updated the app may still be closing; its file goes next time.
pub fn cleanup(dir: &Path) {
    let keep = pending(dir).map(|p| p.file);
    if keep.is_none() {
        let _ = std::fs::remove_file(dir.join(RECORD));
    }
    for folder in [dir.to_path_buf(), std::env::temp_dir()] {
        let ours = folder == dir;
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale = if ours && name.starts_with(INSTALLER_PREFIX) && name.ends_with(".part") {
                true
            } else if let Some(version) = version_of(&name) {
                !newer(version, VERSION) || (ours && keep.as_deref() != Some(name.as_str()))
            } else {
                false
            };
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// ---------------------------------------------------------------- installing

/// Whether this is the installed copy, the one the installer replaces: a
/// build being tested, or a copy anywhere else, never updates itself.
pub fn installed_copy() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    match (exe.parent(), install_dir()) {
        (Some(here), Some(there)) => same_dir(here, &there),
        _ => false,
    }
}

/// Where the installer put the app (its entry in "Apps").
fn install_dir() -> Option<PathBuf> {
    let key = windows_registry::CURRENT_USER
        .open(babutela_buildinfo::uninstall_key())
        .ok()?;
    key.get_string("InstallLocation")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let icon = key.get_string("DisplayIcon").ok()?;
            PathBuf::from(icon.trim().trim_matches('"'))
                .parent()
                .map(Path::to_path_buf)
        })
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether this run may update the app on its own: the installed copy,
/// not started by Windows for a click on a notification (that click waits
/// for this very process), and not a test run, unless it tests the
/// installer (BABUTELA_SETUP_TEST).
pub fn automatic() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        installed_copy()
            && !crate::notify::launched_by_toast()
            && (!crate::smoke::active() || babutela_buildinfo::setup_test())
    })
}

/// Why an update waiting at startup is not run now, if it is not.
pub fn hold_back(p: &Pending, server: &str) -> Option<&'static str> {
    if p.tries >= MAX_TRIES {
        Some("já foi tentada duas vezes")
    } else if p.from.trim_end_matches('/') != server.trim_end_matches('/') {
        Some("veio de outro servidor")
    } else {
        None
    }
}

/// Runs the update waiting in `dir`, after checking its file again and
/// counting the try. `window` shows the installer's progress (the app was
/// on the screen); without it the app opens again in the tray. Whether the
/// installer started, in which case the app must quit at once: it waits for
/// that.
pub fn apply(dir: &Path, mut p: Pending, window: bool) -> bool {
    if !intact(dir, &p) {
        crate::logln!("atualização: o instalador baixado não confere; vou baixar de novo");
        discard(dir, &p);
        return false;
    }
    if !signed(&p.version, &p.name, &p.sha256, &p.sigs) {
        crate::logln!("atualização: o instalador baixado não tem a assinatura do Babu Tela");
        discard(dir, &p);
        return false;
    }
    p.tries += 1;
    if let Err(e) = save(dir, &p) {
        crate::logln!("atualização: anotando a tentativa: {e}");
    }
    let args = Update {
        wait_pid: Some(std::process::id()),
        relaunch: true,
        minimized: !window,
        silent: !window,
    }
    .args();
    crate::logln!(
        "atualização: instalando a {} (tentativa {})",
        p.version,
        p.tries
    );
    run(&dir.join(&p.file), &args)
}

/// Starts the installer, outside any job the app may be in (whose end
/// could take the installer with it), and lets its window come forward.
fn run(installer: &Path, args: &[String]) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let start = |flags: u32| {
        Command::new(installer)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    match start(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| start(CREATE_NO_WINDOW)) {
        Ok(child) => {
            crate::winsys::allow_foreground(child.id());
            true
        }
        Err(e) => {
            crate::logln!("atualização: não deu para abrir o instalador: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------- downloading

/// Looks for a newer version and downloads it into [`dir`], unless it is
/// there already. `found` hears of it as the download starts (the startup
/// screen shows it), and `progress` counts its bytes.
pub async fn fetch(
    shell: &Shell,
    http: &reqwest::Client,
    base: &str,
    progress: &Progress,
    found: impl FnOnce(&str),
    record: &crate::e2e::E2e,
) -> Result<Outcome, String> {
    use std::sync::atomic::Ordering;
    // The check at startup, the one every few hours and the button in the
    // settings end up here, and two downloads of one file would fight.
    let Ok(_busy) = shell.update_op.try_lock() else {
        return Ok(Outcome::Busy);
    };
    let dir = dir(shell);
    let base = base.trim_end_matches('/');
    let feed = format!("{base}/updates/");
    let res = http
        .get(format!("{feed}latest.json"))
        .header("cache-control", "no-cache")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Outcome::None);
    }
    if !res.status().is_success() {
        return Err(format!("resposta {}", res.status().as_u16()));
    }
    let info: LatestInfo = match res.json().await {
        Ok(info @ LatestInfo { .. })
            if !info.file.is_empty()
                && info.sha256.len() == 64
                && info.sha256.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            info
        }
        _ => return Err("arquivo de atualização inválido".into()),
    };
    if !newer(&info.version, VERSION) {
        return Ok(Outcome::None);
    }
    if !signed(&info.version, &info.file, &info.sha256, &info.sigs) {
        return Err("a versão nova não tem as assinaturas do Babu Tela".into());
    }
    // In the public record too (e2e/record.rs), which every app holds and
    // compares: an update made for one person alone would show there. Not
    // checked yet (signed out, or just starting): it waits for a check
    // that can.
    if !record.ready() {
        return Ok(Outcome::None);
    }
    record
        .release_listed(&info.version, &info.file, &info.sha256)
        .await?;
    // Downloaded already, by this run or an earlier one.
    if let Some(p) = pending(&dir)
        && p.version == info.version
        && p.sha256.eq_ignore_ascii_case(&info.sha256)
        && p.name == info.file
        && p.sigs == info.sigs
        && p.from == base
        && intact(&dir, &p)
    {
        return Ok(Outcome::Ready {
            version: p.version,
            tries: p.tries,
        });
    }
    found(&info.version);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = installer_name(&info.version);
    let part = dir.join(format!("{name}.part"));
    let mut bin = http
        .get(format!("{feed}{}", path_escape(&info.file)))
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if bin.status() != reqwest::StatusCode::OK {
        return Err(format!("download falhou ({})", bin.status().as_u16()));
    }
    progress.sent.store(0, Ordering::Relaxed);
    progress
        .total
        .store(bin.content_length().unwrap_or(0), Ordering::Relaxed);
    let mut file = tokio::fs::File::create(&part)
        .await
        .map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut ok = true;
    loop {
        match bin.chunk().await {
            Ok(Some(chunk)) => {
                hash.update(&chunk);
                progress
                    .sent
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                if file.write_all(&chunk).await.is_err() {
                    ok = false;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    ok &= file.flush().await.is_ok();
    drop(file);
    let sha = hex::encode(hash.finalize());
    if !ok || !sha.eq_ignore_ascii_case(&info.sha256) {
        let _ = tokio::fs::remove_file(&part).await;
        return Err("o instalador baixado não confere".into());
    }
    // The one waiting before (an older new version) goes.
    if let Some(old) = pending(&dir)
        && old.file != name
    {
        let _ = std::fs::remove_file(dir.join(&old.file));
    }
    let installer = dir.join(&name);
    let _ = std::fs::remove_file(&installer);
    std::fs::rename(&part, &installer).map_err(|e| e.to_string())?;
    let p = Pending {
        version: info.version,
        file: name,
        sha256: sha,
        from: base.to_owned(),
        tries: 0,
        name: info.file,
        sigs: info.sigs,
    };
    save(&dir, &p).map_err(|e| e.to_string())?;
    Ok(Outcome::Ready {
        version: p.version,
        tries: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_names() {
        assert!(newer("0.9.10", "0.9.2"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.9.2", "0.9.2"));
        assert!(!newer("0.9.1", "0.9.2"));
        assert_eq!(
            path_escape("Babu Tela Setup.exe"),
            "Babu%20Tela%20Setup.exe"
        );
        assert_eq!(path_escape("a/b?c"), "a%2Fb%3Fc");
        assert_eq!(installer_name("0.12.0"), "babutela-update-0.12.0.exe");
        // Whatever the server says, the name stays plain.
        assert_eq!(installer_name("1.0/../x"), "babutela-update-1.0...exe");
        assert_eq!(version_of("babutela-update-0.12.0.exe"), Some("0.12.0"));
        assert_eq!(version_of("babutela-update-.exe"), None);
        assert_eq!(version_of("babutela-update-1;x.exe"), None);
        assert_eq!(version_of("outro.exe"), None);
    }

    /// A version above this one, whatever it is.
    fn next() -> String {
        let mut parts: Vec<u64> = VERSION.split('.').filter_map(|p| p.parse().ok()).collect();
        parts[0] += 1;
        parts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }

    fn folder(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "babutela-teste-atualizacao-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn waiting(dir: &Path, version: &str, bytes: &[u8]) -> Pending {
        let file = installer_name(version);
        std::fs::write(dir.join(&file), bytes).unwrap();
        let p = Pending {
            version: version.into(),
            sha256: sha256_file(&dir.join(&file)).unwrap(),
            file,
            from: "https://servidor.exemplo".into(),
            ..Pending::default()
        };
        save(dir, &p).unwrap();
        p
    }

    #[test]
    fn a_newer_download_waits_and_a_damaged_one_goes() {
        let dir = folder("pendente");
        let p = waiting(&dir, &next(), b"instalador");
        assert_eq!(pending(&dir), Some(p.clone()));
        assert!(intact(&dir, &p));
        assert_eq!(hold_back(&p, "https://servidor.exemplo/"), None);
        assert!(hold_back(&p, "http://127.0.0.1:9").is_some());
        let tired = Pending {
            tries: MAX_TRIES,
            ..p.clone()
        };
        assert!(hold_back(&tired, "https://servidor.exemplo").is_some());
        // Cleaning up keeps it.
        cleanup(&dir);
        assert_eq!(pending(&dir), Some(p.clone()));
        // A changed file is caught before it runs, and forgotten.
        std::fs::write(dir.join(&p.file), b"outra coisa").unwrap();
        assert!(!intact(&dir, &p));
        assert!(!apply(&dir, p, true));
        assert_eq!(pending(&dir), None);
        assert!(!dir.join(RECORD).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_this_version_has_installed_is_cleaned_up() {
        let dir = folder("limpeza");
        // An update to this very version: it just got installed.
        waiting(&dir, VERSION, b"antigo");
        assert_eq!(pending(&dir), None);
        std::fs::write(dir.join(installer_name("0.0.1")), b"x").unwrap();
        std::fs::write(dir.join(format!("{}.part", installer_name(&next()))), b"x").unwrap();
        // An installer nothing points at, for a newer version.
        std::fs::write(dir.join(installer_name(&format!("{}.1", next()))), b"x").unwrap();
        std::fs::write(dir.join("outro.txt"), b"x").unwrap();
        cleanup(&dir);
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["outro.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_that_makes_no_sense_is_ignored() {
        let dir = folder("registro");
        std::fs::write(dir.join(RECORD), b"{ isso nao e json").unwrap();
        assert_eq!(pending(&dir), None);
        // A record naming a file outside the folder's own.
        let p = Pending {
            version: next(),
            file: r"..\..\Windows\notepad.exe".into(),
            sha256: "0".repeat(64),
            ..Pending::default()
        };
        save(&dir, &p).unwrap();
        assert_eq!(pending(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_test_build_is_not_the_installed_copy() {
        // `cargo test` runs from target\debug\deps.
        assert!(!installed_copy());
    }
}
