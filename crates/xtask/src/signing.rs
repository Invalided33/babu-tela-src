//! The keys that sign updates (`cargo xtask chave`): an Ed25519 key kept on
//! each signer's PC, outside build/cloudflare.json and outside the
//! Cloudflare account, in `%USERPROFILE%\.babutela\chave-atualizacoes.txt`
//! (or the file BABUTELA_CHAVE_ATUALIZACOES names).
//!
//! The apps refuse an update whose latest.json is not signed by enough of
//! these keys (crates/app/src/update.rs), so whoever holds the Cloudflare
//! account, or breaks into it, cannot hand everyone an app that sends their
//! keys away. Who the signers are, and how many must sign, is in
//! `assinantes.txt` at the root of the repository (`signers`); without it,
//! the key of this PC alone. With several, no one of them, the one who
//! publishes included, can sign an update alone: the others sign with
//! `cargo xtask assinar` once they have checked it. The public halves go
//! into every build (`BABUTELA_CHAVES_PUBLICAS` and
//! `BABUTELA_ASSINATURAS_MINIMAS`, set by `release`).
//!
//! Keep a copy of the key file somewhere safe and offline. Without enough
//! of the keys, no new version can reach the apps already installed: they
//! would have to be installed again by hand.

use std::path::PathBuf;

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair};

use crate::{Result, fail};

/// What a signature covers: the version, the file and its hash, after a
/// label of its own (the app checks the same string).
pub fn message(version: &str, file: &str, sha256: &str) -> String {
    format!(
        "babutela/atualizacao/v1\n{version}\n{file}\n{}",
        sha256.to_ascii_lowercase()
    )
}

fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("BABUTELA_CHAVE_ATUALIZACOES").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("USERPROFILE").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".babutela").join("chave-atualizacoes.txt")
}

/// The key, if this PC has it.
pub fn load() -> Result<Option<Ed25519KeyPair>> {
    let file = path();
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    let seed = hex::decode(text.trim())
        .ok()
        .filter(|s| s.len() == 32)
        .ok_or_else(|| format!("{} não tem uma chave válida", file.display()))?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| format!("{} não tem uma chave válida", file.display()))?;
    Ok(Some(pair))
}

/// The key, or the reason there is none.
pub fn require() -> Result<Ed25519KeyPair> {
    match load()? {
        Some(pair) => Ok(pair),
        None => fail!(
            "Não achei a chave que assina as atualizações ({}). Crie com: cargo xtask chave",
            path().display()
        ),
    }
}

pub fn public_hex(pair: &Ed25519KeyPair) -> String {
    hex::encode(pair.public_key().as_ref())
}

/// Who signs updates, and how many of them must.
pub struct Signers {
    /// Their public keys, in hex.
    pub keys: Vec<String>,
    pub minimum: usize,
}

/// The file that says who signs: `minimo = N`, then one public key (hex)
/// per line, with a name after it if one likes; `#` starts a comment.
const SIGNERS: &str = "assinantes.txt";

/// The signers in `assinantes.txt` at `root`, or this PC's key alone.
pub fn signers(root: &std::path::Path, own: &Ed25519KeyPair) -> Result<Signers> {
    let file = root.join(SIGNERS);
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(Signers {
            keys: vec![public_hex(own)],
            minimum: 1,
        });
    };
    let mut keys = Vec::new();
    let mut minimum = 1;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(n) = line
            .strip_prefix("minimo")
            .map(|r| r.trim_start_matches([' ', '=']).trim())
        {
            minimum = n
                .parse()
                .map_err(|_| format!("{}: \"minimo\" precisa ser um número", file.display()))?;
            continue;
        }
        let key = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if key.len() != 64 || hex::decode(&key).is_err() {
            fail!(
                "{}: \"{line}\" não é uma chave pública (64 caracteres hexadecimais)",
                file.display()
            );
        }
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if minimum == 0 || minimum > keys.len() {
        fail!(
            "{}: o mínimo ({minimum}) precisa ficar entre 1 e o número de chaves ({})",
            file.display(),
            keys.len()
        );
    }
    Ok(Signers { keys, minimum })
}

/// Whether `sig` is `public`'s signature over this version, file and hash.
pub fn verify(public: &str, version: &str, file: &str, sha256: &str, sig: &str) -> bool {
    let (Ok(public), Ok(sig)) = (hex::decode(public.trim()), hex::decode(sig.trim())) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public)
        .verify(message(version, file, sha256).as_bytes(), &sig)
        .is_ok()
}

/// `cargo xtask assinar <versão> <sha256>`: this PC's signature over that
/// installer, for whoever publishes (who collects them in
/// `build/assinaturas/<versão>.txt`, one line each).
pub fn cosign(args: &[String]) -> Result {
    let [version, sha] = args else {
        fail!("use: cargo xtask assinar <versão> <sha256 do instalador>");
    };
    if sha.len() != 64 || hex::decode(sha).is_err() {
        fail!("o SHA-256 tem 64 caracteres hexadecimais");
    }
    let pair = require()?;
    let line = serde_json::json!({
        "k": public_hex(&pair),
        "s": sign(&pair, version, crate::release::UPDATE_NAME, sha),
    });
    println!("{line}");
    println!(
        "Mande esta linha para quem publica: ela vai em build/assinaturas/{version}.txt. \
         Assine só um instalador que você conferiu (o mesmo SHA-256 que o seu build deu)."
    );
    Ok(())
}

pub fn sign(pair: &Ed25519KeyPair, version: &str, file: &str, sha256: &str) -> String {
    hex::encode(
        pair.sign(message(version, file, sha256).as_bytes())
            .as_ref(),
    )
}

/// `cargo xtask chave`: makes the key the first time, and says where it is
/// and what its public half is.
pub fn run(args: &[String]) -> Result {
    if let Some(extra) = args.first() {
        fail!("opção desconhecida: {extra}");
    }
    let file = path();
    if let Some(pair) = load()? {
        println!("A chave já existe: {}", file.display());
        println!("Pública: {}", public_hex(&pair));
        return Ok(());
    }
    let mut seed = [0u8; 32];
    SystemRandom::new()
        .fill(&mut seed)
        .map_err(|_| "o Windows não deu números aleatórios")?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, hex::encode(seed))?;
    let pair = require()?;
    println!("Chave criada: {}", file.display());
    println!("Pública: {}", public_hex(&pair));
    println!(
        "Guarde uma cópia desse arquivo fora deste PC (um pendrive guardado, por exemplo). \
         Sem ele, nenhuma versão nova chega aos apps instalados."
    );
    Ok(())
}
