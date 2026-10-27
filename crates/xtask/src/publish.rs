//! `cargo xtask publicar`: sends the installer in build/release to the R2
//! bucket, which the server offers at <endereco>/updates/, next to a
//! latest.json with the version, the file name and its SHA-256 (the app
//! refuses anything that does not match). The apps already installed update
//! themselves from there.
//!
//! Bump the version in Cargo.toml ([workspace.package]) before `cargo xtask
//! release`, or the apps will not see a new one.

use std::time::{Duration, SystemTime};

use babutela_buildinfo::VERSION;
use serde_json::{Value, json};

use crate::release::{INSTALLER, STAMP, UPDATE_NAME, release_dir};
use crate::tools::{self, block_on, mb};
use crate::{Result, cf, fail, sigv4};

pub fn run(args: &[String]) -> Result {
    if let Some(extra) = args.first() {
        fail!("opção desconhecida: {extra}");
    }
    let cfg = cf::load()?;
    let dir = release_dir();
    let installer = dir.join(INSTALLER);
    let stamp = dir.join(STAMP);
    if !installer.exists() || !stamp.exists() {
        fail!(
            "Não achei {} e {} em build/release. Rode antes: cargo xtask release",
            INSTALLER,
            STAMP
        );
    }
    // What `release` wrote down about this installer. Checking it keeps two
    // mistakes out: announcing a version the file is not (every app would
    // download it again forever), and sending everyone to another server.
    let built: Value = serde_json::from_str(&std::fs::read_to_string(&stamp)?)?;
    if built["version"] != VERSION {
        fail!(
            "O instalador é da versão {}, mas o Cargo.toml diz {VERSION}. Rode de novo: cargo xtask release",
            built["version"]
        );
    }
    if built["endereco"] != cfg.endereco.as_str() {
        fail!(
            "O instalador aponta para {}, não para {}. Rode de novo: cargo xtask release",
            built["endereco"],
            cfg.endereco
        );
    }
    let modified = |p: &std::path::Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    if modified(&installer) + Duration::from_secs(300) < modified(&stamp) {
        fail!(
            "O instalador é mais antigo que o registro em {STAMP}. Rode de novo: cargo xtask release"
        );
    }

    let bytes = std::fs::read(&installer)?;
    let sha = sigv4::sha256_hex(&bytes);
    // Signed here, with the key that is not in the Cloudflare account, and
    // by the other signers (assinantes.txt), whose lines `cargo xtask
    // assinar` printed: the apps take nothing with fewer (signing.rs).
    let key = crate::signing::require()?;
    let signers = crate::signing::signers(&crate::tools::root(), &key)?;
    let sigs = collect_signatures(&key, &signers, &sha)?;
    let cfg_pepper = cfg
        .secret("PEPPER")
        .ok_or("Falta \"segredos.PEPPER\" em build/cloudflare.json")?
        .to_owned();
    block_on(async {
        let http = tools::http()?;
        let keys = sigv4::keys(&http, &cfg).await?;
        println!("==> Enviando {UPDATE_NAME} ({})", mb(bytes.len() as u64));
        sigv4::put(
            &http,
            &cfg,
            &keys,
            &format!("updates/{UPDATE_NAME}"),
            bytes,
            "application/octet-stream",
        )
        .await?;
        // Into the public record first: the apps install nothing that is
        // not there (crates/app/src/e2e/record.rs).
        let leaf = release_leaf(VERSION, UPDATE_NAME, &sha);
        let logged = http
            .post(format!("{}/api/e2e/log/release", cfg.endereco))
            .json(&json!({
                "version": VERSION, "file": UPDATE_NAME, "sha256": sha,
                "mac": publisher_mac(&cfg_pepper, &leaf),
            }))
            .send()
            .await?;
        if !logged.status().is_success() {
            fail!(
                "O servidor não pôs a versão no registro público ({}): {}",
                logged.status(),
                logged.text().await.unwrap_or_default()
            );
        }
        println!("==> Versão {VERSION} no registro público");
        let latest =
            json!({ "version": VERSION, "file": UPDATE_NAME, "sha256": sha, "sigs": sigs });
        sigv4::put(
            &http,
            &cfg,
            &keys,
            "updates/latest.json",
            latest.to_string().into_bytes(),
            "application/json",
        )
        .await?;
        let check: Value = http
            .get(format!("{}/updates/latest.json", cfg.endereco))
            .header("cache-control", "no-cache")
            .send()
            .await?
            .json()
            .await?;
        if check["sha256"] != sha.as_str() || check["sigs"] != sigs {
            fail!("O servidor devolveu um latest.json diferente do enviado: {check}");
        }
        println!("Versão {VERSION} publicada. Os apps se atualizam ao abrir.");
        Ok(())
    })?
}

/// This PC's signature and the other signers' (one line each from `cargo
/// xtask assinar`, in build/assinaturas/<version>.txt), each checked and
/// counted once: at least the minimum, or nothing goes.
fn collect_signatures(
    own: &ring::signature::Ed25519KeyPair,
    signers: &crate::signing::Signers,
    sha: &str,
) -> Result<Value> {
    let mut sigs = vec![json!({
        "k": crate::signing::public_hex(own),
        "s": crate::signing::sign(own, VERSION, UPDATE_NAME, sha),
    })];
    let file = crate::tools::root()
        .join("build")
        .join("assinaturas")
        .join(format!("{VERSION}.txt"));
    if let Ok(text) = std::fs::read_to_string(&file) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line.trim()).map_err(|e| {
                format!(
                    "{}: linha que não é de `cargo xtask assinar`: {e}",
                    file.display()
                )
            })?;
            sigs.push(v);
        }
    }
    let mut counted: Vec<String> = Vec::new();
    let mut kept = Vec::new();
    for s in sigs {
        let (k, sig) = (s["k"].as_str().unwrap_or(""), s["s"].as_str().unwrap_or(""));
        let k = k.to_ascii_lowercase();
        if !signers.keys.contains(&k) || counted.contains(&k) {
            continue;
        }
        if !crate::signing::verify(&k, VERSION, UPDATE_NAME, sha, sig) {
            fail!("A assinatura de {k} não é deste instalador (SHA-256 {sha})");
        }
        counted.push(k.clone());
        kept.push(json!({ "k": k, "s": sig }));
    }
    if counted.len() < signers.minimum {
        fail!(
            "São {} assinatura(s) de {} necessárias. Peça aos outros assinantes: cargo xtask assinar {VERSION} {sha}, e ponha as linhas em {}",
            counted.len(),
            signers.minimum,
            file.display()
        );
    }
    Ok(Value::Array(kept))
}

/// The version as an entry of the public record (as the Worker and the
/// apps compute it, crates/worker/src/e2e.rs).
fn release_leaf(version: &str, file: &str, sha256: &str) -> String {
    use sha2::{Digest, Sha256};
    let sha = sha256.to_ascii_lowercase();
    let mut h = Sha256::new();
    h.update(b"babutela/e2e/v1/registro/versao");
    for part in [version, file, sha.as_str()] {
        h.update((part.len() as u32).to_be_bytes());
        h.update(part.as_bytes());
    }
    b64(&h.finalize())
}

/// What tells the Worker this comes from whoever publishes: an HMAC of the
/// entry with PEPPER, which only the two of them have.
fn publisher_mac(pepper: &str, leaf: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(pepper.as_bytes()).expect("qualquer tamanho");
    mac.update(b"publicar:");
    mac.update(leaf.as_bytes());
    b64(&mac.finalize().into_bytes())
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
