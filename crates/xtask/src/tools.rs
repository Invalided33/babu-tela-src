//! Running the other tools (cargo, rustup, worker-build, CMake) and the
//! small things every command needs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, fail};

/// The repository root (this crate lives in crates/xtask).
pub fn root() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(Path::parent)
        .unwrap_or(here)
        .to_path_buf()
}

/// The cargo running this, so the same toolchain builds everything.
pub fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// Runs a command, its output going straight to the terminal, and fails
/// when it fails.
pub fn run(cmd: &mut Command, what: &str) -> Result {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd
        .status()
        .map_err(|e| format!("{what}: não consegui rodar {program}: {e}"))?;
    if !status.success() {
        fail!("{what} falhou ({status})");
    }
    Ok(())
}

/// What a command printed, when it ran and succeeded.
pub fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Where NASM is, when it is not on PATH. The video codecs' assembly needs
/// it: rav1d (the AV1 decoder) is not built at all without it, and
/// openh264 would be built in plain C++, encoding several times slower.
fn nasm_dir() -> Option<PathBuf> {
    if output(Command::new("nasm").arg("-v")).is_some() {
        return None;
    }
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let candidates = [
        local.as_ref().map(|l| l.join(r"Programs\NASM")),
        local.as_ref().map(|l| l.join(r"bin\NASM")),
        std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join("NASM")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|dir| dir.join("nasm.exe").exists())
}

/// The PATH to build the app with. libopus (the voice codec) is built with
/// CMake, and the Visual Studio Build Tools ship one even when none is on
/// PATH; vswhere says where. NASM, for the video codecs, is added when it
/// is found off PATH, and without it this fails.
pub fn path_with_cmake() -> Result<OsString> {
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    match nasm_dir() {
        Some(dir) => {
            let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
            dirs.push(dir);
            path = std::env::join_paths(dirs)?;
        }
        None if output(Command::new("nasm").arg("-v")).is_none() => fail!(
            "Não achei o NASM, sem o qual o decodificador de AV1 (rav1d) não compila. Instale \
             ele de https://www.nasm.us (veja o README) ou ponha o nasm no PATH."
        ),
        None => {}
    }
    if output(Command::new("cmake").arg("--version")).is_some() {
        return Ok(path);
    }
    let program_files = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| r"C:\Program Files (x86)".into());
    let vswhere = program_files.join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    let found = output(Command::new(vswhere).args([
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.CMake.Project",
        "-property",
        "installationPath",
    ]));
    if let Some(vs) = found.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
        let bin = Path::new(&vs).join(r"Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin");
        if bin.join("cmake.exe").exists() {
            let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
            dirs.push(bin);
            return Ok(std::env::join_paths(dirs)?);
        }
    }
    fail!(
        "Não achei o CMake, que compila o codec de voz. Instale o Visual Studio Build Tools \
         com \"Desenvolvimento para desktop com C++\" (ele traz o CMake) ou ponha o cmake no PATH."
    )
}

/// `cargo xtask dev`: the app, built and opened in debug mode.
pub fn dev(args: &[String]) -> Result {
    let mut cmd = cargo();
    cmd.current_dir(root())
        .env("PATH", path_with_cmake()?)
        .args(["run", "-p", "babutela", "--"])
        .args(args);
    run(&mut cmd, "o app")
}

/// Runs async code from the synchronous commands.
pub fn block_on<F: std::future::Future>(f: F) -> Result<F::Output> {
    // reqwest and the WebSocket both use rustls, which needs its
    // cryptography picked once, before the first connection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    Ok(rt.block_on(f))
}

/// An HTTP client for talking to Cloudflare and to the server.
pub fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("BabuTela-xtask/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?)
}

/// Asks a question on the terminal and returns the answer, trimmed.
pub fn ask(question: &str) -> String {
    use std::io::Write as _;
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    answer.trim().to_owned()
}

/// Percent-encodes one URL path segment (what encodeURIComponent did).
pub fn escape(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Megabytes, for messages.
pub fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// Whether version `a` is newer than `b` (major.minor.patch), as the app
/// compares them (crates/app/src/update.rs).
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> [u64; 3] {
        let mut out = [0; 3];
        for (slot, p) in out.iter_mut().zip(v.split('.')) {
            *slot = p.parse().unwrap_or(0);
        }
        out
    };
    parts(a) > parts(b)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 with padding (RFC 4648 §4), as GitHub's API takes files.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(BASE64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The other way, ignoring the line breaks GitHub puts in what it sends.
pub fn unbase64(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
    {
        let Some(v) = BASE64.iter().position(|&a| a == c) else {
            fail!("base64 inválido");
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_both_ways() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), coded);
            assert_eq!(unbase64(coded).unwrap(), plain.as_bytes());
        }
        let bytes: Vec<u8> = (0..=255).collect();
        let coded = base64(&bytes);
        // GitHub's way: lines of 60.
        let wrapped: String = coded
            .as_bytes()
            .chunks(60)
            .map(|l| format!("{}\n", String::from_utf8_lossy(l)))
            .collect();
        assert_eq!(unbase64(&wrapped).unwrap(), bytes);
        assert!(unbase64("não").is_err());
    }

    #[test]
    fn versions_compare_like_the_app() {
        assert!(newer("0.11.0", "0.10.1"));
        assert!(newer("0.10.10", "0.10.9"));
        assert!(!newer("0.10.1", "0.10.1"));
        assert!(!newer("0.9.9", "0.10.0"));
    }

    #[test]
    fn segments_are_escaped_like_the_browser_did() {
        assert_eq!(escape("👍"), "%F0%9F%91%8D");
        assert_eq!(escape("Babu Tela Setup.exe"), "Babu%20Tela%20Setup.exe");
        assert_eq!(escape("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn the_root_holds_the_workspace() {
        assert!(root().join("Cargo.toml").exists());
        assert!(root().join("crates").join("worker").exists());
    }
}
