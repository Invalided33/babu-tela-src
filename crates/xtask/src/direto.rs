//! `cargo xtask direto`: a big file sent directly ("Enviar direto", the
//! app's direct/) from one copy of the app to another on this PC, through
//! the server on Cloudflare (in a test database of its own), which only
//! keeps the message and carries the setting up of the connection.
//!
//!   cargo xtask direto              200 MB, straight between the two apps
//!   cargo xtask direto --mb 1000    a file of that many MB
//!   cargo xtask direto --so-tcp     only through the relay over TCP/TLS, as
//!                                   on a network without UDP (it goes
//!                                   through Cloudflare's TURN, which counts
//!                                   against its free 1000 GB a month)
//!   cargo xtask direto --exe target\release\babutela.exe
//!                                   with an app already built
//!
//! Ana attaches the file with "Enviar direto" on and sends it: the message
//! must say what the file is and carry nothing of it. Beto downloads it,
//! pauses half-way, carries on, and must end with the same file, byte for
//! byte; Ana's app stays open until it has gone out whole. The app is built
//! optimized (as `medir` builds it): a debug build's WebRTC moves a couple
//! of MB a second, which says nothing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::call::{launch, wait};
use crate::e2e::T;
use crate::tools::{self, block_on, root};
use crate::{Result, cf, fail};

pub fn run(args: &[String]) -> Result {
    let mut mb = 200u64;
    let mut tcp_only = false;
    let mut given_exe: Option<PathBuf> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--mb" => {
                mb = rest
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|n| (1..=100_000).contains(n))
                    .ok_or("--mb quer um número de 1 a 100000")?;
            }
            "--so-tcp" => tcp_only = true,
            "--exe" => {
                let path = rest.next().ok_or("--exe quer o caminho do app")?;
                given_exe = Some(PathBuf::from(path));
            }
            other => fail!("opção desconhecida: {other}"),
        }
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = cf::load()?;
    let secret = cfg
        .secret("TEST_HUB")
        .ok_or("falta segredos.TEST_HUB em build/cloudflare.json")?
        .to_owned();
    let exe = match given_exe {
        Some(exe) => {
            let exe = if exe.is_absolute() {
                exe
            } else {
                root().join(exe)
            };
            if !exe.is_file() {
                fail!("não achei o app em {}", exe.display());
            }
            println!("==> Usando {}", exe.display());
            exe
        }
        None => {
            println!("==> Compilando o app otimizado (perfil medir)");
            tools::run(
                tools::cargo()
                    .current_dir(root())
                    .env("PATH", tools::path_with_cmake()?)
                    .args(["build", "-p", "babutela", "--profile", "medir"]),
                "o build do app",
            )?;
            root().join("target").join("medir").join("babutela.exe")
        }
    };

    println!("==> Preparando a conversa num banco de teste");
    let t = T::new(cfg.endereco.clone(), &secret)?;
    let room = block_on(t.room(&[("ana", "Ana"), ("beto", "Beto")]))??;
    let dir = std::env::temp_dir().join(format!("babutela-direto-{}", t.run));
    let into = dir.join("baixados");
    std::fs::create_dir_all(&into)?;
    println!("==> Um arquivo de {mb} MB");
    let file = dir.join("grande.bin");
    let sha = write_noise(&file, mb << 20)?;
    let size = mb << 20;

    // Long enough for the slowest way (the relay over TCP, a few MB/s) and
    // a margin.
    let limit = 120 + mb / 2;
    let ana = format!(
        "go:first-server|direto:on|attach:{}|send:|wait-sent:{limit}|mark:enviado|wait-servido:1:{limit}|wait:3000",
        file.display()
    );
    let beto = format!(
        "go:first-server|wait-direto-msg:120|mark:ini|baixar:{}|wait-direto-pct:50:{limit}|mark:meio|pausar-direto|wait:3000|note:pausado|mark:volta|continuar-direto|wait-direto:{limit}|mark:fim|wait:2000",
        into.display()
    );
    let how = if tcp_only {
        "só pelo relé TCP/TLS"
    } else {
        "direto entre os apps"
    };
    println!("==> Ana envia direto, Beto baixa ({how})");
    let seconds = limit + 60;
    let mut children = vec![launch(
        &exe,
        &dir,
        "ana",
        &t.header,
        &room.tokens[0],
        &ana,
        tcp_only,
        "silencio",
        seconds,
        false,
        None,
    )?];
    std::thread::sleep(Duration::from_secs(2));
    children.push(launch(
        &exe,
        &dir,
        "beto",
        &t.header,
        &room.tokens[1],
        &beto,
        tcp_only,
        "silencio",
        seconds,
        false,
        None,
    )?);
    let wait_for = Duration::from_secs(seconds + 120);
    let mut reports = Vec::new();
    for (child, who) in children.iter_mut().zip(["ana", "beto"]) {
        reports.push(wait(child, &dir, who, wait_for)?);
    }
    let (ana, beto) = (&reports[0], &reports[1]);

    let mut problems = Vec::new();
    for (r, who) in [(ana, "Ana"), (beto, "Beto")] {
        if let Some(p) = r["problem"].as_str().filter(|p| !p.is_empty()) {
            problems.push(format!("{who}: {p}"));
        }
        if let Some(missed) = r["misses"].as_array().filter(|m| !m.is_empty()) {
            problems.push(format!("{who}: passos sem efeito: {missed:?}"));
        }
    }
    // The message, as Beto's app opened it: what the file is, and no
    // address (the server only keeps its envelope).
    let attachment = &beto["newest"]["attachments"][0];
    println!("    o anexo, como o app do Beto abriu a mensagem: {attachment}");
    if attachment["kind"] != "direct"
        || attachment["url"] != ""
        || attachment["size"].as_u64() != Some(size)
        || attachment["direct"]["id"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        problems.push(format!(
            "o anexo não veio como arquivo direto: {attachment}"
        ));
    }
    let shared = &ana["direct"]["shared"][0];
    println!(
        "    Ana: compartilhando {} ({} envio(s) inteiro(s), {} bytes enviados agora)",
        shared["name"], ana["direct"]["completed"], shared["sent"]
    );
    if ana["direct"]["completed"].as_u64().unwrap_or(0) < 1 {
        problems.push("o envio da Ana não terminou inteiro".into());
    }
    // Paused half-way, as it was asked.
    let paused = &beto["notes"]["pausado"]["direct"]["downloads"][0];
    let at = paused["done"].as_u64().unwrap_or(0);
    println!(
        "    Beto pausou com {} de {} ({}%), {}",
        tools::mb(at),
        tools::mb(size),
        at * 100 / size.max(1),
        if paused["relayed"] == true {
            "pelo relé"
        } else {
            "direto"
        }
    );
    if paused["state"] != "paused" || at < size / 2 {
        problems.push(format!("o download não estava pausado na metade: {paused}"));
    }
    if (paused["relayed"] == true) != tcp_only {
        problems.push(format!(
            "a conexão foi {}, esperava {}",
            if paused["relayed"] == true {
                "pelo relé"
            } else {
                "direta"
            },
            if tcp_only { "pelo relé" } else { "direta" }
        ));
    }
    // Whole, and the same.
    let got = &beto["direct"]["downloads"][0];
    let path = PathBuf::from(got["path"].as_str().unwrap_or_default());
    if got["state"] != "done" {
        problems.push(format!("o download não terminou: {got}"));
    } else {
        let same = sha256(&path)? == sha;
        println!(
            "    Beto baixou {}: {}",
            path.display(),
            if same {
                "igual ao enviado, byte a byte"
            } else {
                "DIFERENTE DO ENVIADO"
            }
        );
        if !same {
            problems.push("o arquivo baixado não é igual ao enviado".into());
        }
    }
    // How fast, without the pause.
    let mark = |m: &str| beto["marks"][m].as_f64();
    if let (Some(ini), Some(meio), Some(volta), Some(fim)) =
        (mark("ini"), mark("meio"), mark("volta"), mark("fim"))
    {
        let secs = ((meio - ini) + (fim - volta)) / 1000.0;
        println!(
            "    {mb} MB em {secs:.1} s, sem a pausa: {:.1} MB/s",
            mb as f64 / secs.max(0.001)
        );
    }
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(&path);
    if !problems.is_empty() {
        fail!("{}", problems.join("\n"));
    }
    println!("O envio direto funcionou ({how}).");
    Ok(())
}

/// Writes `len` bytes of noise (which no step along the way can shrink) to
/// `path`; their SHA-256.
fn write_noise(path: &Path, len: u64) -> Result<String> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut h = Sha256::new();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut block = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        for c in block.as_chunks_mut::<8>().0 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *c = x.to_le_bytes();
        }
        let n = left.min(block.len() as u64) as usize;
        out.write_all(&block[..n])?;
        h.update(&block[..n]);
        left -= n as u64;
    }
    out.flush()?;
    Ok(hex::encode(h.finalize()))
}

fn sha256(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
