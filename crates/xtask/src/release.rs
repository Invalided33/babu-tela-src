//! `cargo xtask release [--endereco https://outro.servidor]`: builds the
//! installer everyone uses, build/release/Babu Tela Setup.exe.
//!
//! The app talks to the server `cargo xtask servidor` published (the address
//! in build/cloudflare.json), unless another one is given.

use std::path::PathBuf;
use std::time::Instant;

use babutela_buildinfo::VERSION;
use serde_json::json;

use crate::tools::{self, mb, root};
use crate::{Result, cf, fail};

/// The installer's name in build/release.
pub const INSTALLER: &str = "Babu Tela Setup.exe";
/// Its name on the server (no spaces in a URL).
pub const UPDATE_NAME: &str = "Babu-Tela-Setup.exe";
/// What went into the installer, read back by `cargo xtask publicar`.
pub const STAMP: &str = "gerado.json";

pub fn release_dir() -> PathBuf {
    root().join("build").join("release")
}

fn valid_address(url: &str) -> bool {
    let Some(host) = url.strip_prefix("https://") else {
        return false;
    };
    let (name, port) = host.split_once(':').unwrap_or((host, ""));
    let labels: Vec<&str> = name.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_lowercase()))
        && (port.is_empty() || (port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())))
}

pub fn run(args: &[String]) -> Result {
    if args.first().map(String::as_str) == Some("update") {
        return update(&args[1..]);
    }
    build(args)
}

/// `cargo xtask release update`: a new version out, start to end. The
/// installer is built first (nothing goes out if that fails), then the
/// server (the app may need what is new in it) and the emoji lists it
/// keeps, then the update the apps already installed download, and last
/// the download page. Bump the version in Cargo.toml before. It also says
/// whether Ceva published a new noise suppression model (modelo.rs),
/// without stopping for it.
fn update(args: &[String]) -> Result {
    if let Some(extra) = args.first() {
        fail!("opção desconhecida: {extra}");
    }
    // What would stop it halfway, checked before the long build. The R2
    // keys first: the apps' update goes out after the server, so a failure
    // there would leave a new server and the apps without its version.
    let cfg = cf::load()?;
    tools::block_on(async {
        let http = tools::http()?;
        let keys = crate::sigv4::keys(&http, &cfg).await?;
        crate::sigv4::check(&http, &cfg, &keys).await
    })??;
    println!("==> R2: o bucket \"{}\" aceita as chaves", cfg.bucket);
    crate::server::check(&cfg)?;
    let token = crate::site::gh_token()?;
    let live = tools::block_on(live_version(&cfg.endereco))??;
    if let Some(live) = &live
        && !tools::newer(VERSION, live)
    {
        fail!(
            "A versão {VERSION} não é mais nova que a que os apps já têm ({live}). Aumente \
             \"version\" em Cargo.toml ([workspace.package]) e rode de novo."
        );
    }
    crate::modelo::remind();
    println!(
        "==> Publicando a versão {VERSION} (os apps estão na {})",
        live.as_deref()
            .unwrap_or("versão de antes da atualização automática")
    );
    build(&[])?;
    crate::server::run(&[])?;
    // Not worth stopping a release for: without the server's copy the apps
    // read the official addresses, as before.
    let lists = tools::block_on(async {
        let http = tools::http()?;
        let keys = crate::sigv4::keys(&http, &cfg).await?;
        crate::emoji::sync(&http, &cfg, &keys).await
    })
    .and_then(|done| done);
    if let Err(e) = lists {
        println!(
            "ATENÇÃO: a lista de emoji do servidor não foi atualizada ({e}). Até ela estar lá, os \
             apps a buscam no Unicode e no GitHub. Para tentar de novo: cargo xtask emoji"
        );
    }
    crate::publish::run(&[])?;
    let built = crate::site::installer(&cfg)?;
    let page = tools::block_on(crate::site::publish(&cfg, &token, &built))?;
    if let Err(e) = page {
        fail!(
            "o servidor e a atualização automática estão na versão {VERSION}, mas a página de \
             download falhou: {e}\nPara tentar só a página de novo: cargo xtask pagina"
        );
    }
    println!(
        "Versão {VERSION} publicada: servidor, atualização automática dos apps e página de download."
    );
    Ok(())
}

/// The version the apps update to now (updates/latest.json), if any.
async fn live_version(base: &str) -> Result<Option<String>> {
    let res = tools::http()?
        .get(format!("{base}/updates/latest.json"))
        .header("cache-control", "no-cache")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    if res.status().as_u16() == 404 {
        return Ok(None);
    }
    if !res.status().is_success() {
        fail!("{base}/updates/latest.json respondeu {}", res.status());
    }
    let latest: serde_json::Value = res.json().await?;
    Ok(latest["version"].as_str().map(str::to_owned))
}

/// `cargo xtask release [--endereco URL] [--so-instalador]`.
fn build(args: &[String]) -> Result {
    let mut address = None;
    // Rebuilds only the installer around the app built last time: for
    // working on the installer itself.
    let mut installer_only = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--endereco" => address = rest.next().cloned(),
            "--so-instalador" => installer_only = true,
            other => fail!("opção desconhecida: {other}"),
        }
    }
    let address = match address {
        Some(a) => a,
        None => cf::load()?.endereco,
    };
    let url = address.trim().trim_end_matches('/').to_owned();
    if !valid_address(&url) {
        fail!("Endereço inválido: {url} (use https://seu.endereco)");
    }

    let root = root();
    let path = tools::path_with_cmake()?;
    // -trimpath's equivalent: the installer must not carry the paths of
    // this PC. These replace the flags in .cargo/config.toml, so the static
    // C runtime is asked for again.
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let mut flags = vec!["-Ctarget-feature=+crt-static".to_owned()];
    if !home.is_empty() {
        flags.push(format!("--remap-path-prefix={home}=~"));
    }
    flags.push(format!("--remap-path-prefix={}=.", root.display()));
    let flags = flags.join("\x1f");

    let started = Instant::now();
    let release = root.join("target").join("release");
    let payload = release.join("babutela-setup-payload.bin");
    if installer_only && payload.exists() {
        println!("==> Usando o app compilado da última vez");
    } else {
        // The keys the app takes updates from, and how many must sign
        // (signing.rs): without them, an installed app would never update
        // again.
        let signers = crate::signing::signers(&root, &crate::signing::require()?)?;
        println!(
            "==> Compilando o Babu Tela {VERSION} (atualizações: {} de {} assinatura(s))",
            signers.minimum,
            signers.keys.len()
        );
        tools::run(
            tools::cargo()
                .current_dir(&root)
                .env("PATH", &path)
                .env("BABUTELA_SERVER_URL", &url)
                .env("BABUTELA_CHAVES_PUBLICAS", signers.keys.join(","))
                .env("BABUTELA_ASSINATURAS_MINIMAS", signers.minimum.to_string())
                .env("CARGO_ENCODED_RUSTFLAGS", &flags)
                .args(["build", "--release", "--locked", "-p", "babutela"]),
            "o build do app",
        )?;
        let exe = release.join("babutela.exe");
        let app = std::fs::read(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;

        println!("==> Comprimindo o app ({})", mb(app.len() as u64));
        let packed = babutela_setup::payload::pack(&app)?;
        std::fs::write(&payload, &packed)?;
        println!("    {} → {}", mb(app.len() as u64), mb(packed.len() as u64));
    }

    println!("==> Montando o instalador");
    tools::run(
        tools::cargo()
            .current_dir(&root)
            .env("PATH", &path)
            .env("BABUTELA_SETUP_PAYLOAD", &payload)
            .env("CARGO_ENCODED_RUSTFLAGS", &flags)
            .args(["build", "--release", "--locked", "-p", "babutela-setup"]),
        "o build do instalador",
    )?;
    let built = root
        .join("target")
        .join("release")
        .join("babutela-setup.exe");

    // Only now, with a good build in hand: a failure above leaves the
    // previous installer where it was.
    let out = release_dir();
    std::fs::create_dir_all(&out)?;
    for entry in std::fs::read_dir(&out)?.flatten() {
        if entry.path().is_file() {
            std::fs::remove_file(entry.path())?;
        }
    }
    let installer = out.join(INSTALLER);
    std::fs::copy(&built, &installer)?;
    // What went into this installer. Its payload is compressed, so there is
    // no reading this back out of the file; `publicar` checks it before
    // announcing an update.
    let stamp = json!({
        "version": VERSION,
        "endereco": url,
        "gerado": chrono::Local::now().to_rfc3339(),
    });
    std::fs::write(out.join(STAMP), serde_json::to_string_pretty(&stamp)?)?;

    let size = std::fs::metadata(&installer)?.len();
    println!(
        "Pronto em {} s. {INSTALLER}: {} (o app conversa com {url}).",
        started.elapsed().as_secs(),
        mb(size)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::valid_address;

    #[test]
    fn addresses() {
        assert!(valid_address("https://babutela.babutela.workers.dev"));
        assert!(valid_address("https://chat.exemplo.com.br:8443"));
        assert!(!valid_address("http://babutela.workers.dev"));
        assert!(!valid_address("https://localhost"));
        assert!(!valid_address("https://Exemplo.com"));
        assert!(!valid_address("https://exemplo.com/caminho"));
        assert!(!valid_address("https://exemplo.c0m"));
        assert!(!valid_address("https://exemplo.com:123456"));
    }
}
