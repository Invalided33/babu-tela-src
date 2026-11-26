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
//!   cargo xtask direto --so-rele    only through the relay over UDP, as
//!                                   "Esconder meu IP" goes (Cloudflare's
//!                                   TURN too)
//!   cargo xtask direto --rede atraso=20,perda=0.1,banda=100
//!                                   both apps behind a test line (the app's
//!                                   voice/netem.rs: one-way delay in ms,
//!                                   percent lost, upload in Mbit/s, and
//!                                   `fila`, its queue in ms)
//!   --rede-ana SPEC, --rede-beto SPEC
//!                                   one side's line only (Ana sends)
//!   --sem-pausa                     straight through, no pause half-way
//!   --cc reno|cubic|bbr             SCTP's congestion control in both apps,
//!                                   in place of the usual (BBR for files,
//!                                   Reno in calls), to compare them
//!   --em-chamada                    Ana and Beto in a call first, each
//!                                   hearing the other's beeps (as `medir
//!                                   voz`): 15 s of the call alone, then the
//!                                   file goes while it lasts, and the voice's
//!                                   delay is measured in both (no pause)
//!   --slides N                      no file: in the call, Ana presents a PDF
//!                                   of N pages of noise (pictures as big as
//!                                   photos get), which go to Beto over the
//!                                   call's data channel: how long they take,
//!                                   and the voice's delay meanwhile
//!   --exe target\release\babutela.exe
//!                                   with an app already built
//!
//! Ana attaches the file with "Enviar direto" on and sends it: the message
//! must say what the file is and carry nothing of it. Beto downloads it,
//! pauses half-way, carries on, and must end with the same file, byte for
//! byte; Ana's app stays open until it has gone out whole. The app is built
//! optimized (as `medir` builds it): a debug build's WebRTC moves a couple
//! of MB a second, which says nothing.
//!
//! Both apps measure their side of the connection every second (the app's
//! direct/meter.rs: SCTP's congestion window and round trip, its
//! retransmissions, what each waited on): the seconds and the totals are
//! printed at the end, so a slow transfer says why it is slow. The last
//! line, `RESULTADO {...}`, has the numbers of the run in one JSON, for a
//! series of runs to collect.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::call::{launch_with, wait};
use crate::e2e::T;
use crate::tools::{self, block_on, root};
use crate::{Result, cf, fail};

/// In a call (`--em-chamada`): time for both apps to start, sign in, join
/// the call and connect before its first measured moment; then how long
/// the call goes alone before the file.
const CALL_SETUP_MS: f64 = 25_000.0;
const BASE_MS: f64 = 15_000.0;

/// The way the connection is made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    Direct,
    RelayUdp,
    RelayTcp,
}

pub fn run(args: &[String]) -> Result {
    let mut mb = 200u64;
    let mut way = Way::Direct;
    let mut given_exe: Option<PathBuf> = None;
    let mut both: Option<String> = None;
    let mut lines: [Option<String>; 2] = [None, None];
    let mut pause = true;
    let mut cc: Option<String> = None;
    let mut in_call = false;
    let mut slides: Option<usize> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--slides" => {
                let n = rest
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|n| (1..=50).contains(n))
                    .ok_or("--slides quer um número de 1 a 50")?;
                slides = Some(n);
            }
            "--cc" => {
                let name = rest
                    .next()
                    .filter(|n| ["reno", "cubic", "bbr"].contains(&n.as_str()))
                    .ok_or("--cc quer reno, cubic ou bbr")?;
                cc = Some(name.clone());
            }
            "--em-chamada" => in_call = true,
            "--mb" => {
                mb = rest
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|n| (1..=100_000).contains(n))
                    .ok_or("--mb quer um número de 1 a 100000")?;
            }
            "--so-tcp" => way = Way::RelayTcp,
            "--so-rele" => way = Way::RelayUdp,
            "--sem-pausa" => pause = false,
            "--rede" => both = Some(rest.next().ok_or("--rede quer a linha")?.clone()),
            "--rede-ana" => lines[0] = Some(rest.next().ok_or("--rede-ana quer a linha")?.clone()),
            "--rede-beto" => {
                lines[1] = Some(rest.next().ok_or("--rede-beto quer a linha")?.clone());
            }
            "--exe" => {
                let path = rest.next().ok_or("--exe quer o caminho do app")?;
                given_exe = Some(PathBuf::from(path));
            }
            other => fail!("opção desconhecida: {other}"),
        }
    }
    for line in &mut lines {
        if line.is_none() {
            line.clone_from(&both);
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
    if let Some(pages) = slides {
        return present(&exe, &t, &room, &dir, pages, &lines, cc.as_deref());
    }
    println!("==> Um arquivo de {mb} MB");
    let file = dir.join("grande.bin");
    let sha = write_noise(&file, mb << 20)?;
    let size = mb << 20;

    // Long enough for a slow line (a few MB/s, or less on a bad test line)
    // and a margin.
    let limit = 120 + mb * 2;
    // In a call: both join it and take the steps at the same moments of
    // the clock, once there has been time to start and connect; the call
    // alone (`base`), then the file while it goes (`carga`).
    if in_call {
        pause = false;
    }
    let start = crate::call::wall_ms() + CALL_SETUP_MS;
    let until = |ms: f64| format!("until:{:.0}", start + ms);
    let call_steps = || format!("join|{}|mark:base|{}|go:text|", until(0.0), until(BASE_MS));
    let in_call_first = if in_call { call_steps() } else { String::new() };
    let ana = format!(
        "go:first-server|{in_call_first}direto:on|attach:{}|send:|wait-sent:{limit}|mark:carga|wait-servido:1:{limit}|mark:fim|wait:3000",
        file.display()
    );
    // The pause right as half has come (the steps go 1.5 s apart; a fast
    // line would be done by the next one).
    let beto = if in_call {
        format!(
            "go:first-server|{in_call_first}wait-direto-msg:120|mark:carga|baixar:{}|wait-direto:{limit}|mark:fim|wait:2000",
            into.display()
        )
    } else if pause {
        format!(
            "go:first-server|wait-direto-msg:120|mark:ini|baixar:{}|wait-direto-pct:50:{limit}|pausar-direto|mark:meio|wait:3000|note:pausado|mark:volta|continuar-direto|wait-direto:{limit}|mark:fim|wait:2000",
            into.display()
        )
    } else {
        format!(
            "go:first-server|wait-direto-msg:120|mark:ini|baixar:{}|wait-direto:{limit}|mark:fim|wait:2000",
            into.display()
        )
    };
    let how = match way {
        Way::Direct => "direto entre os apps",
        Way::RelayUdp => "só pelo relé UDP",
        Way::RelayTcp => "só pelo relé TCP/TLS",
    };
    let line_of = |i: usize| lines[i].as_deref().unwrap_or("sem rede de teste");
    println!(
        "==> Ana envia direto, Beto baixa ({how}; Ana: {}; Beto: {}; controle: {}{})",
        line_of(0),
        line_of(1),
        cc.as_deref().unwrap_or("o de sempre"),
        if in_call { "; numa chamada" } else { "" }
    );
    let seconds = limit + 60 + if in_call { 60 } else { 0 };
    let env_of = |i: usize| {
        let mut env: Vec<(&str, String)> = vec![("BABUTELA_DIRETO_LOG", "1".into())];
        if let Some(line) = &lines[i] {
            env.push(("BABUTELA_REDE_TESTE", line.clone()));
        }
        if way == Way::RelayUdp {
            env.push(("BABUTELA_TURN_SO_UDP", "1".into()));
        }
        if let Some(cc) = &cc {
            env.push(("BABUTELA_SCTP_CC", cc.clone()));
        }
        env
    };
    let mut children = Vec::new();
    for (i, (who, steps)) in [("ana", &ana), ("beto", &beto)].into_iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        children.push(launch_with(
            &exe,
            &dir,
            who,
            &t.header,
            &room.tokens[i],
            steps,
            way == Way::RelayTcp,
            // In a call, each one's voice beeps at every second of the
            // clock, and the other times when it hears them (measuring).
            if in_call { "relogio" } else { "silencio" },
            seconds,
            in_call,
            None,
            &env_of(i),
        )?);
    }
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
    let relayed_wanted = way != Way::Direct;
    if pause {
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
        // A fast line can have it all there by the time the pause comes
        // (the steps go 1.5 s apart): then it must end whole all the same,
        // not stay paused (checked below with the rest).
        let late = at >= size;
        if late {
            println!(
                "    a pausa chegou com o arquivo já todo no Beto: a linha é rápida demais para \
                 testar a pausa (use --sem-pausa, um arquivo maior ou uma linha mais lenta)"
            );
        }
        if !late && (paused["state"] != "paused" || at < size / 2) {
            problems.push(format!("o download não estava pausado na metade: {paused}"));
        }
        if (paused["relayed"] == true) != relayed_wanted {
            problems.push(format!(
                "a conexão foi {}, esperava {}",
                if paused["relayed"] == true {
                    "pelo relé"
                } else {
                    "direta"
                },
                if relayed_wanted {
                    "pelo relé"
                } else {
                    "direta"
                }
            ));
        }
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
    let began = if in_call { "carga" } else { "ini" };
    let secs = match (mark(began), mark("meio"), mark("volta"), mark("fim")) {
        (Some(ini), Some(meio), Some(volta), Some(fim)) => {
            Some(((meio - ini) + (fim - volta)) / 1000.0)
        }
        (Some(ini), _, _, Some(fim)) if !pause => Some((fim - ini) / 1000.0),
        _ => None,
    };
    if let Some(secs) = secs {
        println!(
            "    {mb} MB em {secs:.1} s{}: {:.1} MB/s",
            if pause { ", sem a pausa" } else { "" },
            mb as f64 / secs.max(0.001)
        );
    }
    // How each side's connection went.
    let sends = ana["direct"]["medidasEnvios"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let fetched = &got["medidas"];
    for (who, m) in [
        ("Ana (envio)", sends.last()),
        ("Beto (download)", Some(fetched)),
    ] {
        if let Some(r) = m.map(|m| &m["resumo"]).filter(|r| !r.is_null()) {
            println!("    {who}: {r}");
        }
    }
    if let Some(last) = sends.last() {
        timeline(&last["segundos"]);
    }
    let voice = if in_call {
        voice_delays(ana, beto)
    } else {
        Value::Null
    };
    // The run in one line, for a series of runs to collect.
    let sent = sends.last().map_or(Value::Null, |m| m["resumo"].clone());
    let resent =
        sent["reenvios_rapidos"].as_u64().unwrap_or(0) + sent["reenvios"].as_u64().unwrap_or(0);
    let result = serde_json::json!({
        "cc": cc.as_deref().unwrap_or("padrao"),
        "controle": sent["controle"],
        "caminho": how,
        "rede_ana": lines[0],
        "rede_beto": lines[1],
        "mb": mb,
        // From "Baixar" to the end, connecting included.
        "segundos": secs,
        "mbs": secs.map(|s| mb as f64 / s.max(0.001)),
        // From the first byte to the last at Beto (MB of 10^6 bytes): the
        // connection's own speed, without connecting or the disk's flush.
        "dados_s": fetched["resumo"]["dados_s"],
        "mbs_dados": fetched["resumo"]["mbs_dados"],
        "srtt_ms": sent["srtt_ms"],
        "srtt_p95_ms": sent["srtt_p95_ms"],
        "reenvios": resent,
        // Of the DATA chunks (about 1200 bytes of the file each).
        "reenvio_pct": resent as f64 * 1200.0 * 100.0 / size.max(1) as f64,
        "timeouts": sent["timeouts"],
        "voz": voice,
        "ok": problems.is_empty(),
    });
    println!("RESULTADO {result}");
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(&path);
    if !problems.is_empty() {
        fail!("{}", problems.join("\n"));
    }
    println!("O envio direto funcionou ({how}).");
    Ok(())
}

/// `--slides`: in a call, Ana presents a PDF of `pages` pages of noise and
/// Beto gets the pictures over the call's data channel (the app's slides/):
/// how long until he has them all, and the voice's delay meanwhile.
fn present(
    exe: &PathBuf,
    t: &T,
    room: &crate::e2e::Room,
    dir: &Path,
    pages: usize,
    lines: &[Option<String>; 2],
    cc: Option<&str>,
) -> Result {
    let pdf = dir.join("ruido.pdf");
    let pdf_bytes = write_noisy_pdf(&pdf, pages)?;
    let line_of = |i: usize| lines[i].as_deref().unwrap_or("sem rede de teste");
    println!(
        "==> Numa chamada, Ana apresenta {pages} slides de ruído (PDF de {}) e o Beto recebe \
         (Ana: {}; Beto: {}; controle: {})",
        tools::mb(pdf_bytes),
        line_of(0),
        line_of(1),
        cc.unwrap_or("o de sempre")
    );
    let limit = 300u64;
    let start = crate::call::wall_ms() + CALL_SETUP_MS;
    let until = |ms: f64| format!("until:{:.0}", start + ms);
    let call = format!(
        "go:first-server|join|{}|mark:base|{}|",
        until(0.0),
        until(BASE_MS)
    );
    // Ana stays until every slide went to Beto, and a little more for the
    // last pieces on their way.
    let ana = format!(
        "{call}apresentar:{}|mark:carga|wait-slides:{limit}|wait:5000|mark:fim|wait:2000",
        pdf.display()
    );
    let beto = format!("{call}mark:carga|wait-slides:{limit}|mark:fim|wait:2000");
    let seconds = limit + 120;
    let mut children = Vec::new();
    for (i, (who, steps)) in [("ana", &ana), ("beto", &beto)].into_iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        let mut env: Vec<(&str, String)> = Vec::new();
        if let Some(line) = &lines[i] {
            env.push(("BABUTELA_REDE_TESTE", line.clone()));
        }
        if let Some(cc) = cc {
            env.push(("BABUTELA_SCTP_CC", cc.to_owned()));
        }
        children.push(launch_with(
            exe,
            dir,
            who,
            &t.header,
            &room.tokens[i],
            steps,
            false,
            "relogio",
            seconds,
            true,
            None,
            &env,
        )?);
    }
    let wait_for = Duration::from_secs(seconds + 120);
    let mut reports = Vec::new();
    for (child, who) in children.iter_mut().zip(["ana", "beto"]) {
        reports.push(wait(child, dir, who, wait_for)?);
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
    let mine = &ana["slides"]["mine"];
    let theirs = &beto["slides"]["theirs"][0];
    let count = mine["count"].as_u64().unwrap_or(0);
    let pictures = mine["bytes"].as_u64().unwrap_or(0);
    println!(
        "    Ana desenhou {} slides ({} de figuras); chegaram {} de {} ao Beto",
        mine["drawn"],
        tools::mb(pictures),
        theirs["received"],
        theirs["count"]
    );
    if count != pages as u64 || theirs["received"].as_u64() != Some(count) {
        problems.push(format!("os slides não chegaram todos: {theirs}"));
    }
    let mark = |m: &str| beto["marks"][m].as_f64();
    let secs = match (mark("carga"), mark("fim")) {
        (Some(from), Some(to)) => Some((to - from) / 1000.0),
        _ => None,
    };
    if let Some(secs) = secs {
        println!(
            "    do início da apresentação a todos os slides no Beto: {secs:.1} s ({:.2} MB/s)",
            pictures as f64 / 1e6 / secs.max(0.001)
        );
    }
    let drawing = &ana["medidas"]["series"]["carga:slides.desenho"];
    println!("    Ana: desenhar um slide levou p50 {} ms", drawing["p50"]);
    let voice = voice_delays(ana, beto);
    let result = serde_json::json!({
        "cc": cc.unwrap_or("padrao"),
        "slides": count,
        "mb_figuras": pictures as f64 / 1e6,
        "rede_ana": lines[0],
        "rede_beto": lines[1],
        "segundos": secs,
        "mbs": secs.map(|s| pictures as f64 / 1e6 / s.max(0.001)),
        "desenho_p50_ms": drawing["p50"],
        "voz": voice,
        "ok": problems.is_empty(),
    });
    println!("RESULTADO {result}");
    if !problems.is_empty() {
        fail!("{}", problems.join("\n"));
    }
    println!("A apresentação chegou inteira.");
    Ok(())
}

/// A PDF of `pages` 16:9 pages, each one picture of grey noise at full HD:
/// what the app draws of each is a big JPEG, as a photo would be, which no
/// compressor shrinks. Its size in bytes.
fn write_noisy_pdf(path: &Path, pages: usize) -> Result<u64> {
    const W: usize = 1920;
    const H: usize = 1080;
    let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
    // Where each object starts, by its number (from 1).
    let mut at: Vec<usize> = Vec::new();
    let mut object = |out: &mut Vec<u8>, head: String, stream: Option<&[u8]>| {
        at.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{head}\n", at.len()).as_bytes());
        if let Some(data) = stream {
            out.extend_from_slice(b"stream\n");
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream\n");
        }
        out.extend_from_slice(b"endobj\n");
    };
    // 1 the catalog, 2 the page tree, then three for each page: the page,
    // its picture and what draws it.
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 3 + 3 * i)).collect();
    object(&mut out, "<< /Type /Catalog /Pages 2 0 R >>".into(), None);
    object(
        &mut out,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            kids.join(" ")
        ),
        None,
    );
    let draw: &[u8] = b"q 960 0 0 540 0 0 cm /Im0 Do Q";
    let mut picture = vec![0u8; W * H];
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for i in 0..pages {
        let (image, contents) = (4 + 3 * i, 5 + 3 * i);
        object(
            &mut out,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 960 540] \
                 /Resources << /XObject << /Im0 {image} 0 R >> >> /Contents {contents} 0 R >>"
            ),
            None,
        );
        for c in picture.as_chunks_mut::<8>().0 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *c = x.to_le_bytes();
        }
        object(
            &mut out,
            format!(
                "<< /Type /XObject /Subtype /Image /Width {W} /Height {H} \
                 /ColorSpace /DeviceGray /BitsPerComponent 8 /Length {} >>",
                picture.len()
            ),
            Some(&picture),
        );
        object(
            &mut out,
            format!("<< /Length {} >>", draw.len()),
            Some(draw),
        );
    }
    let xref = out.len();
    let mut table = format!("xref\n0 {}\n0000000000 65535 f \n", at.len() + 1);
    for offset in &at {
        table.push_str(&format!("{offset:010} 00000 n \n"));
    }
    table.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        at.len() + 1
    ));
    out.extend_from_slice(table.as_bytes());
    std::fs::write(path, &out)?;
    Ok(out.len() as u64)
}

/// In a call: the voice's delay, mouth to ear, as each one heard the other
/// with the call alone (`base`) and while the file went (`carga`), and how
/// often the sound ran dry. Beto hears Ana, whose upload carries the file.
fn voice_delays(ana: &Value, beto: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (who, name, r) in [("beto", "Beto", beto), ("ana", "Ana", ana)] {
        let mut phases = serde_json::Map::new();
        for phase in ["base", "carga"] {
            let s = &r["medidas"]["series"][format!("{phase}:voz.boca_ouvido")];
            let count = |name: &str| {
                r["medidas"]["counts"][format!("{phase}:voz.{name}")]
                    .as_u64()
                    .unwrap_or(0)
            };
            let (dry, lost, got) = (count("secou"), count("perdidos"), count("pacotes"));
            // Of the voice's packets that should have come, the share that
            // never did (each one heard is a packet, each gap the lost).
            let lost_pct = 100.0 * lost as f64 / (lost + got).max(1) as f64;
            let ms = |k: &str| s[k].as_f64();
            let show = |v: Option<f64>| v.map_or_else(|| "?".to_owned(), |v| format!("{v:.0}"));
            println!(
                "    {name} ouve ({phase}): boca a ouvido p50 {} · p95 {} · máx {} ms (n={}) · \
                 o som secou {dry} vez(es) · {lost_pct:.1} % da voz perdida ({lost} de {})",
                show(ms("p50")),
                show(ms("p95")),
                show(ms("max")),
                s["n"],
                lost + got
            );
            phases.insert(
                phase.into(),
                serde_json::json!({
                    "p50": ms("p50"),
                    "p95": ms("p95"),
                    "max": ms("max"),
                    "n": s["n"],
                    "secou": dry,
                    "perdidos": lost,
                    "pacotes": got,
                    "perda_pct": lost_pct,
                }),
            );
        }
        out.insert(who.into(), Value::Object(phases));
    }
    Value::Object(out)
}

/// The sender's seconds, a few lines' worth: how fast, SCTP's window and
/// round trip, and the losses it saw.
fn timeline(seconds: &Value) {
    let Some(list) = seconds.as_array().filter(|l| !l.is_empty()) else {
        return;
    };
    let every = list.len().div_ceil(30).max(1);
    println!(
        "    {:>6} {:>7} {:>9} {:>7} {:>8} {:>8} {:>8} {:>6} {:>6} {:>6} {:>6} {:>4} {:>5}",
        "t (s)",
        "MB/s",
        "controle",
        "banda",
        "janela",
        "limite",
        "no ar",
        "srtt",
        "rttmin",
        "perdas",
        "reenv",
        "t3",
        "rede%"
    );
    for chunk in list.chunks(every) {
        let last = &chunk[chunk.len() - 1];
        let sum = |k: &str| chunk.iter().filter_map(|s| s[k].as_u64()).sum::<u64>();
        let mean = |k: &str| {
            let v: Vec<f64> = chunk.iter().filter_map(|s| s[k].as_f64()).collect();
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };
        let kb = |k: &str| format!("{:.0}K", mean(k));
        // BBR's bound after a probe lost too much, as the last second had
        // it ("-": none).
        let bound = last["limite_kb"]
            .as_f64()
            .filter(|b| *b > 0.0)
            .map_or_else(|| "-".to_owned(), |b| format!("{b:.0}K"));
        println!(
            "    {:>6} {:>7.2} {:>9} {:>7.2} {:>8} {:>8} {:>8} {:>6.0} {:>6.1} {:>6} {:>6} {:>4} {:>4.0}%",
            last["t"]
                .as_f64()
                .map_or_else(|| "?".into(), |t| format!("{t:.0}")),
            mean("mbs"),
            last["controle"].as_str().unwrap_or("?"),
            mean("banda_mbs"),
            kb("janela_kb"),
            bound,
            kb("no_ar_kb"),
            mean("srtt_ms"),
            mean("rtt_min_ms"),
            sum("perdidos") + sum("recuperacoes"),
            sum("reenvios_rapidos") + sum("reenvios"),
            sum("timeouts"),
            mean("rede_pct"),
        );
    }
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
