//! `cargo xtask medir`: how fast and how light the app is, on this PC,
//! against the server on Cloudflare (each part in a test database of its
//! own). The app copies run their test mode with BABUTELA_MEDIR=1, so they
//! time what they do themselves (crates/app/src/probe.rs), and this reads
//! their processor time and memory from Windows four times a second.
//!
//!   cargo xtask medir                 everything below, in order
//!   cargo xtask medir mensagens       a message: through the server alone,
//!                                     then from one app to the other
//!   cargo xtask medir ocioso          an app left open in a text channel
//!   cargo xtask medir figurinhas      the sticker picker open, its stickers
//!                                     moving (from a fake GIPHY on this PC),
//!                                     then stickers in the chat
//!   cargo xtask medir voz             a call, from mouth to ear
//!   cargo xtask medir tela            a shared window, from the screen of
//!                                     one to the screen of the other
//!   cargo xtask medir midia           a 720p video attachment playing in
//!                                     the chat
//!   cargo xtask medir arquivos        a big file up and back down
//!   cargo xtask medir captura         (only when asked) the whole screen
//!                                     shared three ways in one call:
//!                                     Windows Graphics Capture, DXGI, and
//!                                     everything on the graphics card
//!   cargo xtask medir qualidades      (only when asked) the whole screen
//!                                     shared on the graphics card at each
//!                                     quality, with nobody watching and
//!                                     then with someone
//!   --segundos N                      how long each measured phase lasts
//!   --rotulo NOME                     keeps the numbers in
//!                                     build/medidas/NOME.json
//!   --comparar NOME                   and shows them next to that file's
//!   --exe CAMINHO                     measures an app already built (the
//!                                     one in the installer) instead
//!   --ruido ia|padrao|desligada       the noise suppression in `voz` (as
//!                                     the settings pick it; the app's own
//!                                     choice, the network, if not given)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::e2e::{Room, T};
use crate::tools::{self, block_on, root};
use crate::{Result, cf, fail};

/// User name and display name of each copy of the app measured.
const PEOPLE: [(&str, &str); 6] = [
    ("ana", "Ana"),
    ("beto", "Beto"),
    ("caio", "Caio"),
    ("dani", "Dani"),
    ("eva", "Eva"),
    ("fabio", "Fábio"),
];

/// Where the two windows go, side by side, so neither covers the other.
const LEFT: &str = "place:0:40:950:760";
const RIGHT: &str = "place:960:40:950:760";

const PARTS: [&str; 7] = [
    "mensagens",
    "ocioso",
    "figurinhas",
    "voz",
    "tela",
    "midia",
    "arquivos",
];

/// Asked for by name only (not part of a plain `medir`): experiments.
const EXTRA_PARTS: [&str; 2] = ["captura", "qualidades"];

/// The numbers of one run, by name (milliseconds, percent, MB, MB/s).
type Numbers = BTreeMap<String, f64>;

struct Setup {
    base: String,
    secret: String,
    exe: PathBuf,
    seconds: u64,
    /// --env NAME=VALUE: extra environment for the app copies (to compare
    /// settings, as WGPU_BACKEND=dx12).
    env: Vec<(String, String)>,
    /// --giphy: `figurinhas` against the real GIPHY, with this key (from
    /// build/cloudflare.json, so it is never typed on a command line).
    giphy_key: Option<String>,
    /// --ruido: the noise suppression the call in `voz` goes through.
    noise: Option<String>,
}

pub fn run(args: &[String]) -> Result {
    let mut parts: Vec<&'static str> = Vec::new();
    let mut seconds = 20u64;
    let mut label: Option<String> = None;
    let mut compare: Option<String> = None;
    let mut env: Vec<(String, String)> = Vec::new();
    // An app already built (the release's, to test what goes out).
    let mut given_exe: Option<PathBuf> = None;
    let mut real_giphy = false;
    let mut noise: Option<String> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--ruido" => {
                let n = rest
                    .next()
                    .filter(|n| ["ia", "padrao", "desligada"].contains(&n.as_str()))
                    .ok_or("--ruido quer ia, padrao ou desligada")?;
                noise = Some(n.clone());
            }
            "--exe" => {
                let path = rest.next().ok_or("--exe quer o caminho do app")?;
                given_exe = Some(PathBuf::from(path));
            }
            "--segundos" => {
                seconds = rest
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|s| (5..=120).contains(s))
                    .ok_or("--segundos quer um número de 5 a 120")?;
            }
            "--rotulo" => label = Some(rest.next().ok_or("--rotulo quer um nome")?.clone()),
            "--env" => {
                let pair = rest.next().ok_or("--env quer NOME=VALOR")?;
                let (k, v) = pair.split_once('=').ok_or("--env quer NOME=VALOR")?;
                env.push((k.to_owned(), v.to_owned()));
            }
            "--comparar" => {
                compare = Some(rest.next().ok_or("--comparar quer um nome")?.clone());
            }
            "--giphy" => real_giphy = true,
            other => match PARTS.iter().chain(&EXTRA_PARTS).find(|p| **p == other) {
                Some(p) => parts.push(p),
                None => fail!("opção desconhecida: {other}"),
            },
        }
    }
    if parts.is_empty() {
        parts = PARTS.to_vec();
    }
    // The HTTP clients are made outside `block_on`, which would otherwise
    // pick rustls' cryptography first.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = cf::load()?;
    let secret = cfg
        .secret("TEST_HUB")
        .ok_or("falta segredos.TEST_HUB em build/cloudflare.json")?
        .to_owned();
    let giphy_key = if real_giphy {
        let key = cfg
            .secret("GIPHY_API_KEY")
            .ok_or("--giphy quer segredos.GIPHY_API_KEY em build/cloudflare.json")?;
        Some(key.to_owned())
    } else {
        None
    };

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
    let setup = Setup {
        base: cfg.endereco.clone(),
        secret,
        exe,
        seconds,
        env,
        giphy_key,
        noise,
    };
    let mut numbers = Numbers::new();
    for part in parts {
        match part {
            "mensagens" => messages(&setup, &mut numbers)?,
            "ocioso" => idle(&setup, &mut numbers)?,
            "figurinhas" => stickers(&setup, &mut numbers)?,
            "voz" => voice(&setup, &mut numbers)?,
            "tela" => screen(&setup, &mut numbers)?,
            "midia" => media(&setup, &mut numbers)?,
            "captura" => capture(&setup, &mut numbers)?,
            "qualidades" => qualities(&setup, &mut numbers)?,
            _ => files(&setup, &mut numbers)?,
        }
    }
    save(label, compare.as_deref(), &numbers)
}

// ---------------------------------------------------------------- the parts

fn messages(s: &Setup, out: &mut Numbers) -> Result {
    println!("==> Mensagens");
    let (t, room, dir) = room(s, 6, 40)?;
    // Where the time goes before any message: to Cloudflare and back, to
    // the test database (made near this PC) and to the real one.
    let [edge, test_hub, real_hub] = block_on(round_trips(&t, &s.base))??;
    // With R2 on, a missing file is a trip to the bucket, not just the edge.
    print_line("ida e volta: Worker e R2 (arquivo que não existe)", &edge);
    print_line("ida e volta: banco de teste", &test_hub);
    print_line("ida e volta: banco de verdade", &real_hub);
    keep(out, "rede.cloudflare", &edge);
    keep(out, "rede.banco_teste", &test_hub);
    keep(out, "rede.banco_real", &real_hub);

    let [http, echo, other, typing, gw_echo, gw_other] = block_on(t.time_messages(&room, 15))??;
    let (http, echo, other, typing) = (stats(&http), stats(&echo), stats(&other), stats(&typing));
    let (gw_echo, gw_other) = (stats(&gw_echo), stats(&gw_other));
    print_line("só o servidor: resposta do envio", &http);
    print_line("só o servidor: volta para quem mandou", &echo);
    print_line("só o servidor: chega para o outro", &other);
    print_line("só o servidor: \"digitando\" chega para o outro", &typing);
    print_line(
        "só o servidor, pelo gateway: volta para quem mandou",
        &gw_echo,
    );
    print_line("só o servidor, pelo gateway: chega para o outro", &gw_other);
    keep(out, "mensagens.servidor.resposta", &http);
    keep(out, "mensagens.servidor.eco", &echo);
    keep(out, "mensagens.servidor.entrega", &other);
    keep(out, "mensagens.servidor.digitando", &typing);
    keep(out, "mensagens.servidor.gateway_eco", &gw_echo);
    keep(out, "mensagens.servidor.gateway_entrega", &gw_other);

    let n = 20u64;
    let busy = 3000 + n * 1100 + 3000;
    let beto = format!(
        "{RIGHT}|go:first-server|mark:ini|wait:{}|mark:fim",
        busy + 3000
    );
    let ana =
        format!("{LEFT}|go:first-server|wait:3000|mark:ini|burst:{n}:1100|wait:3000|mark:fim");
    let limit = busy / 1000 + 90;
    let mut copies = vec![
        launch(s, &t, &dir, "beto", &room.tokens[1], &beto, None, limit)?,
        launch(s, &t, &dir, "ana", &room.tokens[0], &ana, None, limit)?,
    ];
    let reports = finish(&mut copies, &dir, limit)?;
    let (beto, ana) = (&reports[0], &reports[1]);
    for (label, report, series) in [
        ("entre apps: resposta do envio", ana, "ini:msg.http"),
        ("entre apps: volta para quem mandou", ana, "ini:msg.eco"),
        (
            "entre apps: aparece na tela de quem mandou",
            ana,
            "ini:msg.local",
        ),
        ("entre apps: chega para o outro", beto, "ini:msg.entrega"),
        ("entre apps: aparece na tela do outro", beto, "ini:msg.tela"),
    ] {
        let found = find(report, series);
        print_line(label, &found);
        keep(out, &format!("mensagens.app.{}", short(series)), &found);
    }
    usage_lines(out, "mensagens", &copies, &reports, &[("ini", "fim")]);
    Ok(())
}

fn idle(s: &Setup, out: &mut Numbers) -> Result {
    println!("==> Ocioso (o app aberto num canal de texto)");
    let (t, room, dir) = room(s, 3, 30)?;
    let steps = format!(
        "{LEFT}|go:first-server|wait:3000|mark:ini|wait:{}|mark:fim",
        s.seconds * 1000
    );
    let limit = s.seconds + 90;
    let mut copies = vec![launch(
        s,
        &t,
        &dir,
        "ana",
        &room.tokens[0],
        &steps,
        None,
        limit,
    )?];
    let reports = finish(&mut copies, &dir, limit)?;
    usage_lines(out, "ocioso", &copies, &reports, &[("ini", "fim")]);
    Ok(())
}

/// The sticker picker open on trending, its stickers moving, then a search
/// and two stickers shown in the chat: what the stickers cost the app.
/// Against a fake GIPHY on this PC (crates/xtask/src/giphy_fake.rs), with
/// no GIPHY key needed; with --giphy, against GIPHY itself (a handful of
/// the key's calls), and the pictures go to build/giphy/, for GIPHY's
/// review of the production key. The last picture shows both.
fn stickers(s: &Setup, out: &mut Numbers) -> Result {
    let fake = match &s.giphy_key {
        Some(_) => {
            println!("==> Figurinhas (o seletor aberto e animado, contra o GIPHY de verdade)");
            None
        }
        None => {
            println!(
                "==> Figurinhas (o seletor aberto e animado, com um GIPHY de mentira neste PC)"
            );
            Some(crate::giphy_fake::start()?)
        }
    };
    let (t, room, dir) = room(s, 3, 10)?;
    let mut env = s.env.clone();
    match (&fake, &s.giphy_key) {
        (Some(fake), _) => {
            env.push(("BABUTELA_GIPHY_URL".into(), fake.base.clone()));
            env.push(("BABUTELA_GIPHY_KEY".into(), "chave-de-teste".into()));
        }
        (None, Some(key)) => env.push(("BABUTELA_GIPHY_KEY".into(), key.clone())),
        (None, None) => {}
    }
    let with_giphy = Setup {
        base: s.base.clone(),
        secret: s.secret.clone(),
        exe: s.exe.clone(),
        seconds: s.seconds,
        env,
        giphy_key: None,
        noise: s.noise.clone(),
    };
    // Two stickers of trending in the chat: the fake's by name, GIPHY's by
    // their place in the list.
    let (first, second) = if fake.is_some() {
        ("babu05", "babu12")
    } else {
        ("#0", "#5")
    };
    // Open on trending (measured), then closed with two stickers in the
    // chat (measured once the picker's animations had time to go), then
    // open again on a search, for the picture.
    let steps = format!(
        "{LEFT}|animar|go:first-server|pop:sticker|sticker-tab:em-alta|wait-anims:20:60|mark:ini|\
         wait:{}|mark:fim|sticker-message:{first}|sticker-message:{second}|pop:close|wait:25000|\
         mark:fechado|wait:{}|mark:fechado-fim|note:fechado|pop:sticker|sticker-search:gato|\
         wait-anims:20:30|wait:1500",
        s.seconds * 1000,
        s.seconds * 1000
    );
    let limit = 2 * s.seconds + 150;
    let mut copies = vec![launch(
        &with_giphy,
        &t,
        &dir,
        "ana",
        &room.tokens[0],
        &steps,
        None,
        limit,
    )?];
    let reports = finish(&mut copies, &dir, limit)?;
    let ana = &reports[0];
    let g = &ana["giphy"];
    let n = |v: &Value| v.as_f64().unwrap_or(0.0);
    println!(
        "    animações na memória: {} ({} quadros, {:.1} MB de texturas) · falharam: {}",
        g["anims"],
        g["frames"],
        n(&g["pixels"]) * 4.0 / 1e6,
        g["failedAnims"]
    );
    // Frames drawn while open: one per frame of the stickers (theirs last
    // 60 ms here), at most 20 a second.
    let drawn = find(ana, "ini:ui.intervalo").map_or(0.0, |v| n(&v["n"]));
    let open_s = (n(&ana["marks"]["fim"]) - n(&ana["marks"]["ini"])) / 1000.0;
    println!(
        "    em alta: {} figurinhas · busca \"{}\": {} · com o seletor aberto, {:.1} quadros \
         da janela por segundo",
        g["trending"]["items"],
        g["search"]["query"].as_str().unwrap_or(""),
        g["search"]["items"],
        drawn / open_s.max(1.0)
    );
    match &fake {
        Some(fake) => println!(
            "    pedidos ao GIPHY: em alta {} · busca {} · por id {} · arquivos {} · registros {}",
            fake.asked("/v1/stickers/trending"),
            fake.asked("/v1/stickers/search"),
            fake.asked("/v1/gifs"),
            fake.asked("/m/"),
            fake.asked("/ping")
        ),
        None => println!(
            "    pedidos à API do GIPHY (os que a chave conta): {} · registros de visto/clicado/\
             enviado: {} · o GIPHY conhece este usuário de teste por {}",
            g["calls"],
            n(&g["pings"]["seen"]) + n(&g["pings"]["click"]) + n(&g["pings"]["sent"]),
            g["customerId"].as_str().unwrap_or("")
        ),
    }
    let closed = &ana["notes"]["fechado"]["giphy"];
    println!(
        "    com o seletor fechado: {} animações na memória ({:.1} MB de texturas)",
        closed["anims"],
        n(&closed["pixels"]) * 4.0 / 1e6
    );
    out.insert("figurinhas.animacoes".into(), n(&g["anims"]));
    out.insert("figurinhas.texturas_mb".into(), n(&g["pixels"]) * 4.0 / 1e6);
    out.insert(
        "figurinhas.fechado.texturas_mb".into(),
        n(&closed["pixels"]) * 4.0 / 1e6,
    );
    usage_lines(
        out,
        "figurinhas",
        &copies,
        &reports,
        &[("ini", "fim"), ("fechado", "fechado-fim")],
    );
    if n(&g["failedAnims"]) > 0.0 || n(&g["anims"]) < 20.0 {
        println!("    aviso: nem todas as figurinhas apareceram");
    }
    if let Some(shot) = ana["screenshot"].as_str() {
        println!("    imagem: {shot}");
        if fake.is_none() {
            // For GIPHY's review: the picker, its mark and stickers sent.
            let folder = root().join("build").join("giphy");
            std::fs::create_dir_all(&folder)?;
            let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S");
            let kept = folder.join(format!("seletor-{stamp}.png"));
            std::fs::copy(shot, &kept)?;
            println!("    guardada para a revisão do GIPHY: {}", kept.display());
        }
    }
    Ok(())
}

fn media(s: &Setup, out: &mut Numbers) -> Result {
    println!("==> Mídia (um vídeo 720p de 30 quadros por segundo tocando no chat)");
    let (t, room, dir) = room(s, 3, 10)?;
    // Made by the app itself, with Windows' encoder, the first time.
    let video = root().join("build").join("medidas").join("teste-720p.mp4");
    let steps = format!(
        "{LEFT}|go:first-server|video-teste:{v}|media:{v}|play:0|mark:ini|wait:{}|mark:fim",
        (s.seconds * 1000).min(10_000),
        v = video.display()
    );
    let limit = s.seconds + 120;
    let mut copies = vec![launch(
        s,
        &t,
        &dir,
        "ana",
        &room.tokens[0],
        &steps,
        None,
        limit,
    )?];
    let reports = finish(&mut copies, &dir, limit)?;
    let ana = &reports[0];
    for (label, series) in [
        ("cor do quadro (decodificador)", "ini:midia.quadro"),
        ("textura (interface)", "ini:midia.textura"),
        ("entre um quadro da janela e outro", "ini:ui.intervalo"),
    ] {
        let found = find(ana, series);
        print_line(label, &found);
        keep(out, &format!("midia.{}", short(series)), &found);
    }
    let shown = ana["medidas"]["counts"]["ini:midia.quadros"]
        .as_f64()
        .unwrap_or(0.0);
    let passes = ana["medidas"]["counts"]["ini:ui.passadas"]
        .as_f64()
        .unwrap_or(0.0);
    println!("    quadros mostrados: {shown:.0} · passadas da interface: {passes:.0}");
    out.insert("midia.quadros".into(), shown);
    out.insert("midia.passadas".into(), passes);
    usage_lines(out, "midia", &copies, &reports, &[("ini", "fim")]);
    Ok(())
}

fn voice(s: &Setup, out: &mut Numbers) -> Result {
    println!("==> Voz (duas pessoas; cada uma ouve os bipes da outra)");
    let (t, room, dir) = room(s, 2, 0)?;
    let phase = s.seconds * 1000;
    // Both at the same moments of the clock.
    let start = wall_ms() + 20_000.0;
    let at = |ms: u64| format!("until:{:.0}", start + ms as f64);
    let noise = s
        .noise
        .as_ref()
        .map(|n| format!("ruido:{n}|"))
        .unwrap_or_default();
    let steps = |place: &str| {
        format!(
            "{place}|go:first-server|{noise}join|{}|mark:palco|{}|go:text|{}|mark:texto|{}|mark:fim",
            at(0),
            at(phase),
            at(phase + 2000),
            at(2 * phase + 2000),
        )
    };
    let limit = 2 * s.seconds + 60;
    let mut copies = vec![
        launch(
            s,
            &t,
            &dir,
            "ana",
            &room.tokens[0],
            &steps(LEFT),
            Some("relogio"),
            limit,
        )?,
        launch(
            s,
            &t,
            &dir,
            "beto",
            &room.tokens[1],
            &steps(RIGHT),
            Some("relogio"),
            limit,
        )?,
    ];
    let reports = finish(&mut copies, &dir, limit)?;
    for (who, report) in ["Ana", "Beto"].iter().zip(&reports) {
        for phase in ["palco", "texto"] {
            let heard = find(report, &format!("{phase}:voz.boca_ouvido"));
            print_line(&format!("{who} ouve ({phase}): boca a ouvido"), &heard);
            keep(
                out,
                &format!("voz.{phase}.boca_ouvido.{}", who.to_lowercase()),
                &heard,
            );
        }
        println!(
            "    {who}: supressão de ruído {}",
            report["noise"]["chosen"].as_str().unwrap_or("?")
        );
        for (label, series) in [
            ("fila de quem chega", "palco:voz.fila"),
            ("atraso do microfone", "palco:voz.entrada"),
            ("atraso do alto-falante", "palco:voz.saida"),
            ("rede de ruído, por 10 ms", "palco:voz.ruido"),
        ] {
            let found = find(report, series);
            print_line(&format!("{who}: {label}"), &found);
            keep(
                out,
                &format!("voz.{}.{}", short(series), who.to_lowercase()),
                &found,
            );
        }
        let dry = report["medidas"]["counts"]["palco:voz.secou"]
            .as_u64()
            .unwrap_or(0);
        println!("    {who}: o som secou {dry} vezes");
        out.insert(format!("voz.secou.{}", who.to_lowercase()), dry as f64);
    }
    usage_lines(
        out,
        "voz",
        &copies,
        &reports,
        &[("palco", "texto"), ("texto", "fim")],
    );
    Ok(())
}

fn screen(s: &Setup, out: &mut Numbers) -> Result {
    println!("==> Tela (a Ana compartilha a própria janela; o Beto vê grande e depois pequeno)");
    let (t, room, dir) = room(s, 2, 0)?;
    let phase = s.seconds * 1000;
    // Then, without the stamp (whose redrawing would weigh on Ana's
    // processor), Ana shares the whole screen, which has Beto's window on
    // it showing that same screen: it changes all the time, like a video.
    // It goes the usual way, on the graphics card when it can (while
    // measuring, `share:screen` would take the processor path).
    // Both take their steps at the same moments of the clock, with time
    // after each change for it to settle.
    let start = wall_ms() + 25_000.0;
    let at = |ms: u64| format!("until:{:.0}", start + ms as f64);
    let (p, settle) = (phase, 4000);
    // The changes happen in phases of their own ("troca"), kept out of
    // the numbers.
    // Without the PC's sound: on one PC, Ana's whole screen would take
    // Beto's call (and Beto play it back to her), and the numbers are of
    // the picture.
    let ana = format!(
        "{LEFT}|go:first-server|join|share-audio:off|share:self|{}|mark:grande|{}|mark:troca1\
         |{}|mark:pequena|{}|mark:troca2|share:screen-zero|{}|mark:monitor|{}|mark:fim",
        at(0),
        at(p),
        at(p + settle),
        at(2 * p + settle),
        at(2 * p + 2 * settle),
        at(3 * p + 2 * settle),
    );
    let beto = format!(
        "{RIGHT}|go:first-server|join|zoom:ana|{}|mark:grande|{}|mark:troca1|zoom:|{}\
         |mark:pequena|{}|mark:troca2|zoom:ana|{}|mark:monitor|{}|mark:fim",
        at(0),
        at(p),
        at(p + settle),
        at(2 * p + settle),
        at(2 * p + 2 * settle),
        at(3 * p + 2 * settle),
    );
    let limit = 3 * s.seconds + 60;
    let mut copies = vec![
        launch(s, &t, &dir, "ana", &room.tokens[0], &ana, None, limit)?,
        launch(s, &t, &dir, "beto", &room.tokens[1], &beto, None, limit)?,
    ];
    let reports = finish(&mut copies, &dir, limit)?;
    let (ana, beto) = (&reports[0], &reports[1]);
    for phase in ["grande", "pequena", "monitor"] {
        println!("    {phase}:");
        for (label, report, series) in [
            ("da tela da Ana à tela do Beto", beto, "tela.na_tela"),
            ("até decodificar", beto, "tela.decodificada"),
            ("Ana: leitura da captura", ana, "video.captura"),
            ("Ana: espera do codificador", ana, "video.espera"),
            ("Ana: conversão", ana, "video.conversao"),
            ("Ana: codificação", ana, "video.codificacao"),
            ("Ana: espera do ritmo", ana, "video.ritmo"),
            ("Ana: da captura à rede", ana, "video.saida"),
            ("Beto: montagem do quadro", beto, "video.montagem"),
            ("Beto: decodificação", beto, "video.decodificacao"),
            ("Beto: cor e textura", beto, "video.cor"),
        ] {
            let found = find(report, &format!("{phase}:{series}"));
            print_line(&format!("  {label}"), &found);
            keep(out, &format!("tela.{phase}.{}", short(series)), &found);
        }
        for (label, series) in [
            ("quadro-chave", "video.chave_kb"),
            ("quadro", "video.quadro_kb"),
        ] {
            if let Some(v) = find(ana, &format!("{phase}:{series}")) {
                println!(
                    "      Ana: {label} p50 {:.0} KB · máx {:.0} KB ({} deles)",
                    v["p50"].as_f64().unwrap_or(0.0),
                    v["max"].as_f64().unwrap_or(0.0),
                    v["n"]
                );
            }
        }
        let shown = find(beto, &format!("{phase}:video.intervalo"))
            .and_then(|v| v["n"].as_f64())
            .unwrap_or(0.0);
        // Over the phase as Beto's marks measure it (the steps between the
        // marks make it longer than --segundos).
        let end = match phase {
            "grande" => "troca1",
            "pequena" => "troca2",
            _ => "fim",
        };
        let span = beto["marks"][end].as_f64().unwrap_or(0.0)
            - beto["marks"][phase].as_f64().unwrap_or(0.0);
        let fps = if span > 0.0 {
            shown / (span / 1000.0)
        } else {
            0.0
        };
        let width = find(beto, &format!("{phase}:video.largura"))
            .and_then(|v| v["p50"].as_f64())
            .unwrap_or(0.0);
        println!("      Beto recebeu {fps:.1} quadros por segundo, de {width:.0} px de largura");
        // The small copy is at most 640 wide: a wider picture means the
        // phase measured the wrong copy (a click on a test window changes
        // what Beto shows).
        if (phase == "pequena") != (width <= 640.0) {
            println!("      aviso: essa não é a cópia {phase}; repita sem mexer nas janelas");
        }
        out.insert(format!("tela.{phase}.fps"), fps);
    }
    usage_lines(
        out,
        "tela",
        &copies,
        &reports,
        &[
            ("grande", "troca1"),
            ("pequena", "troca2"),
            ("monitor", "fim"),
        ],
    );
    for (who, report) in [("Ana", ana), ("Beto", beto)] {
        for phase in ["grande", "monitor"] {
            let frames = find(report, &format!("{phase}:ui.intervalo"));
            print_line(
                &format!("{who} ({phase}): entre um quadro da janela e outro"),
                &frames,
            );
        }
    }
    Ok(())
}

/// The ways of sharing a whole screen compared by `captura`: the phase's
/// name, and the smoke step that starts it.
const CAPTURES: [(&str, &str); 3] = [
    // Windows Graphics Capture, then conversion and openh264 on the
    // processor (what screens get without an encoder on the card).
    ("wgc", "share:screen"),
    // The same through Desktop Duplication.
    ("dxgi", "share:screen-dxgi"),
    // Capture, conversion and encoder all on the graphics card (what
    // screens get when it can).
    ("zero", "share:screen-zero"),
];

/// The same whole screen shared in each of the CAPTURES ways, one after
/// the other in the same call: Ana's window spans the top of the screen
/// with the time stamped across it, Beto's sits below, showing the screen
/// enlarged (and so itself, which keeps the picture changing).
fn capture(s: &Setup, out: &mut Numbers) -> Result {
    println!("==> Captura (a tela inteira, por cada caminho, na mesma chamada)");
    let (t, room, dir) = room(s, 2, 0)?;
    let (p, settle) = (s.seconds * 1000, 5000);
    let start = wall_ms() + 25_000.0;
    let at = |ms: u64| format!("until:{:.0}", start + ms as f64);
    // Both above every other window: the screen has to show them, not
    // whatever else is open.
    // Without the PC's sound, as in `screen`.
    let mut ana =
        "place:0:0:1920:620|topo|go:first-server|join|carimbo:on|share-audio:off".to_owned();
    let mut beto = "place:480:640:960:400|topo|go:first-server|join|zoom:ana".to_owned();
    let mut phases = Vec::new();
    for (i, (name, share)) in CAPTURES.iter().enumerate() {
        let begin = i as u64 * (p + settle);
        let end = if i + 1 == CAPTURES.len() {
            "fim".to_owned()
        } else {
            format!("troca{}", i + 1)
        };
        ana += &format!(
            "|{share}|{}|mark:{name}|{}|mark:{end}",
            at(begin),
            at(begin + p)
        );
        beto += &format!(
            "|zoom:ana|{}|mark:{name}|{}|mark:{end}",
            at(begin),
            at(begin + p)
        );
        phases.push((*name, end));
    }
    let limit = CAPTURES.len() as u64 * (s.seconds + 5) + 90;
    let mut copies = vec![
        launch(s, &t, &dir, "ana", &room.tokens[0], &ana, None, limit)?,
        launch(s, &t, &dir, "beto", &room.tokens[1], &beto, None, limit)?,
    ];
    let reports = finish(&mut copies, &dir, limit)?;
    let (ana, beto) = (&reports[0], &reports[1]);
    for (phase, end) in &phases {
        println!("    {phase}:");
        for (label, report, series) in [
            ("da tela da Ana à tela do Beto", beto, "tela.na_tela"),
            ("até decodificar", beto, "tela.decodificada"),
            ("Ana: leitura da captura", ana, "video.captura"),
            ("Ana: conversão", ana, "video.conversao"),
            ("Ana: codificação", ana, "video.codificacao"),
            ("Ana: quadro (KB)", ana, "video.quadro_kb"),
            ("Ana: quadro cheio (KB)", ana, "video.chave_kb"),
            ("Ana: da captura à rede", ana, "video.saida"),
            ("Beto: decodificação", beto, "video.decodificacao"),
            ("Beto: cor e textura", beto, "video.cor"),
        ] {
            let found = find(report, &format!("{phase}:{series}"));
            print_line(&format!("  {label}"), &found);
            keep(out, &format!("captura.{phase}.{}", short(series)), &found);
        }
        let span = |r: &Value| {
            r["marks"][end.as_str()].as_f64().unwrap_or(0.0)
                - r["marks"][*phase].as_f64().unwrap_or(0.0)
        };
        let per_second = |r: &Value, series: &str| {
            let n = find(r, &format!("{phase}:{series}"))
                .and_then(|v| v["n"].as_f64())
                .unwrap_or(0.0);
            if span(r) > 0.0 {
                n / (span(r) / 1000.0)
            } else {
                0.0
            }
        };
        let (read, shown) = (
            per_second(ana, "video.captura"),
            per_second(beto, "video.intervalo"),
        );
        let width = find(beto, &format!("{phase}:video.largura"))
            .and_then(|v| v["p50"].as_f64())
            .unwrap_or(0.0);
        println!(
            "      Ana leu {read:.1} imagens por segundo; Beto recebeu {shown:.1}, de {width:.0} px"
        );
        out.insert(format!("captura.{phase}.lidas_por_s"), read);
        out.insert(format!("captura.{phase}.fps"), shown);
    }
    let spans: Vec<(&str, &str)> = phases.iter().map(|(p, e)| (*p, e.as_str())).collect();
    usage_lines(out, "captura", &copies, &reports, &spans);
    Ok(())
}

/// The share picker's qualities, as `qualidades` goes through them.
const QUALITIES: [&str; 4] = ["720p30", "1080p30", "1080p60", "source"];

/// The whole screen shared on the graphics card at each quality, in one
/// call: first with nobody watching and Ana's call view closed (a text
/// channel open, as when the window is put aside), then with Beto
/// watching it enlarged and Ana's call view open.
fn qualities(s: &Setup, out: &mut Numbers) -> Result {
    println!(
        "==> Qualidades (a tela inteira pela placa: sem ninguém assistindo, depois com o Beto)"
    );
    let (t, room, dir) = room(s, 2, 0)?;
    let (p, settle) = (s.seconds * 1000, 6000);
    let start = wall_ms() + 30_000.0;
    let at = |ms: u64| format!("until:{:.0}", start + ms as f64);
    // Without the PC's sound, as in `screen`.
    let mut ana = format!("{LEFT}|go:first-server|join|share-audio:off|share:screen-zero|go:text");
    let mut beto = format!("{RIGHT}|go:first-server");
    let mut phases = Vec::new();
    let mut clock = 0;
    for watched in [false, true] {
        if watched {
            // Beto comes into the call and enlarges Ana's screen; she opens
            // the call view.
            ana += &format!("|{}|go:voice", at(clock));
            beto += &format!("|{}|join|zoom:ana", at(clock));
            clock += 10_000;
        }
        for q in QUALITIES {
            let name = format!("{q}.{}", if watched { "assistida" } else { "sozinha" });
            let end = format!("troca{}", phases.len() + 1);
            // Both mark the phases: each app keeps its numbers by its own.
            let marks = format!(
                "|{}|mark:{name}|{}|mark:{end}",
                at(clock + settle),
                at(clock + settle + p)
            );
            ana += &format!("|qualidade:{q}{marks}");
            beto += &marks;
            phases.push((name, end));
            clock += settle + p;
        }
    }
    let limit = clock / 1000 + 120;
    let mut copies = vec![
        launch(s, &t, &dir, "ana", &room.tokens[0], &ana, None, limit)?,
        launch(s, &t, &dir, "beto", &room.tokens[1], &beto, None, limit)?,
    ];
    let reports = finish(&mut copies, &dir, limit)?;
    let (ana, beto) = (&reports[0], &reports[1]);
    for (phase, end) in &phases {
        let span = |r: &Value| {
            r["marks"][end.as_str()].as_f64().unwrap_or(0.0)
                - r["marks"][phase.as_str()].as_f64().unwrap_or(0.0)
        };
        let count = |r: &Value, series: &str| {
            find(r, &format!("{phase}:{series}"))
                .and_then(|v| v["n"].as_f64())
                .unwrap_or(0.0)
        };
        let seconds = span(ana) / 1000.0;
        let per_second = |n: f64| if seconds > 0.0 { n / seconds } else { 0.0 };
        let width = find(beto, &format!("{phase}:video.largura"))
            .and_then(|v| v["p50"].as_f64())
            .unwrap_or(0.0);
        let encode = find(ana, &format!("{phase}:video.codificacao"))
            .and_then(|v| v["p50"].as_f64())
            .unwrap_or(0.0);
        let (read, sent) = (
            per_second(count(ana, "video.captura")),
            per_second(count(ana, "video.codificacao")),
        );
        println!(
            "    {phase}: Ana leu {read:.0} imagens por segundo e codificou {sent:.1} \
             (mediana {encode:.2} ms); Beto recebeu {width:.0} px"
        );
        out.insert(format!("qualidades.{phase}.lidas_por_s"), read);
        out.insert(format!("qualidades.{phase}.codificadas_por_s"), sent);
    }
    let spans: Vec<(&str, &str)> = phases
        .iter()
        .map(|(p, e)| (p.as_str(), e.as_str()))
        .collect();
    usage_lines(out, "qualidades", &copies, &reports, &spans);
    Ok(())
}

fn files(s: &Setup, out: &mut Numbers) -> Result {
    println!("==> Arquivos (64 MB)");
    let (t, room, dir) = room(s, 2, 0)?;
    let size = 64 * 1024 * 1024;
    let data = noise(size);
    let [up, first, down] = block_on(t.time_file(&room.tokens[0], &data))??;
    let mb = size as f64 / (1024.0 * 1024.0);
    println!(
        "    só o servidor: envio {:.1} MB/s · primeiro byte de volta {:.0} ms · download {:.1} MB/s",
        mb / up,
        first * 1000.0,
        mb / down
    );
    out.insert("arquivos.servidor.envio_mb_s".into(), mb / up);
    out.insert("arquivos.servidor.primeiro_byte".into(), first * 1000.0);
    out.insert("arquivos.servidor.download_mb_s".into(), mb / down);
    // As the app sends (net.rs PARTS_AT_ONCE).
    let [up, down, again] = block_on(t.time_file_together(&room.tokens[0], &data, 6))??;
    println!(
        "    só o servidor, 6 partes juntas: envio {:.1} MB/s · download em pedaços {:.1} MB/s \
         · inteiro de novo {:.1} MB/s",
        mb / up,
        mb / down,
        mb / again
    );
    out.insert("arquivos.servidor.envio_juntas_mb_s".into(), mb / up);
    out.insert("arquivos.servidor.download_pedacos_mb_s".into(), mb / down);
    out.insert("arquivos.servidor.download_de_novo_mb_s".into(), mb / again);

    let path = dir.join("medida.bin");
    std::fs::write(&path, &data)?;
    let steps = format!(
        "{LEFT}|go:first-server|attach:{}|mark:ini|send:|wait-sent:600|mark:fim",
        path.display()
    );
    let limit = 700;
    let mut copies = vec![launch(
        s,
        &t,
        &dir,
        "ana",
        &room.tokens[0],
        &steps,
        None,
        limit,
    )?];
    let reports = finish(&mut copies, &dir, limit)?;
    match find(&reports[0], "ini:arquivo.envio").and_then(|v| v["p50"].as_f64()) {
        Some(ms) => {
            let rate = mb / (ms / 1000.0);
            // The parts went up together: the file must still be the same,
            // byte for byte.
            let url = reports[0]["newest"]["attachments"][0]["url"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let intact = !url.is_empty() && block_on(t.holds(&url, &data))??;
            println!(
                "    pelo app: envio {rate:.1} MB/s ({:.1} s), {}",
                ms / 1000.0,
                if intact {
                    "o arquivo voltou igual"
                } else {
                    "O ARQUIVO NÃO VOLTOU IGUAL"
                }
            );
            if !intact {
                fail!("o arquivo enviado pelo app não voltou igual");
            }
            out.insert("arquivos.app.envio_mb_s".into(), rate);
        }
        None => println!(
            "    pelo app: o envio não terminou ({})",
            reports[0]["problem"]
        ),
    }
    usage_lines(out, "arquivos", &copies, &reports, &[("ini", "fim")]);
    let _ = std::fs::remove_file(&path);
    Ok(())
}

// ---------------------------------------------------------------- the server

/// A fresh test database with `people` in one server, `chatter` messages
/// in its text channel (from all but the first two, who are measured), and
/// a folder for the apps' profiles and reports.
fn room(s: &Setup, people: usize, chatter: usize) -> Result<(T, Room, PathBuf)> {
    let t = T::new(s.base.clone(), &s.secret)?;
    let room = block_on(async {
        let room = t.room(&PEOPLE[..people]).await?;
        if chatter > 0 {
            let talkers = room.tokens[2.min(people - 1)..].to_vec();
            t.fill_chat(&room, &talkers, chatter).await?;
        }
        Ok::<_, Box<dyn std::error::Error>>(room)
    })??;
    let dir = std::env::temp_dir().join(format!("babutela-medir-{}", t.run));
    std::fs::create_dir_all(&dir)?;
    Ok((t, room, dir))
}

/// Ten round trips each: to the Worker alone (a file that does not exist),
/// to this run's test database, and to the real one.
async fn round_trips(t: &T, base: &str) -> Result<[Option<Value>; 3]> {
    let http = tools::http()?;
    let missing = format!("{base}/uploads/{}.txt", "0".repeat(32));
    let version = format!("{base}/api/version");
    let mut times = [Vec::new(), Vec::new(), Vec::new()];
    for _ in 0..10 {
        let started = Instant::now();
        http.get(&missing).send().await?;
        times[0].push(started.elapsed().as_secs_f64() * 1000.0);
        let started = Instant::now();
        t.get("", "/version").await?;
        times[1].push(started.elapsed().as_secs_f64() * 1000.0);
        let started = Instant::now();
        http.get(&version).send().await?;
        times[2].push(started.elapsed().as_secs_f64() * 1000.0);
    }
    // The first of each opened its connection.
    Ok(times.map(|v| stats(&v[1..])))
}

// ---------------------------------------------------------------- the apps

/// What Windows says one process used so far.
#[derive(Clone, Copy, Debug)]
struct Sample {
    /// When (ms since 1970, the clock the apps stamp their marks with).
    at: f64,
    cpu_ms: f64,
    private_mb: f64,
    working_mb: f64,
    /// In RAM and not shared with other processes: Task Manager's "Memory".
    own_mb: f64,
}

struct Copy {
    who: &'static str,
    child: Child,
    samples: Vec<Sample>,
}

fn wall_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

#[allow(clippy::too_many_arguments)]
fn launch(
    s: &Setup,
    t: &T,
    dir: &Path,
    who: &'static str,
    token: &str,
    steps: &str,
    voice: Option<&str>,
    limit_s: u64,
) -> Result<Copy> {
    let mut cmd = Command::new(&s.exe);
    cmd.env("PROSA_USERDATA", dir.join(format!("perfil-{who}")))
        .env("PROSA_NO_TRAY", "1")
        .env("BABUTELA_TEST_HUB", &t.header)
        .env("PROSA_SMOKE_TOKEN", token)
        .env("PROSA_SMOKE_ACTIONS", steps)
        .env("PROSA_SMOKE_SECONDS", limit_s.to_string())
        .env("BABUTELA_MEDIR", "1")
        .arg(format!(
            "--smoke-report={}",
            dir.join(format!("{who}.json")).display()
        ));
    if let Some(voice) = voice {
        cmd.env("BABUTELA_TESTE_VOZ", voice);
    }
    for (k, v) in &s.env {
        cmd.env(k, v);
    }
    Ok(Copy {
        who,
        child: cmd.spawn()?,
        samples: Vec::new(),
    })
}

/// Samples the copies until they all end, then reads their reports.
fn finish(copies: &mut [Copy], dir: &Path, limit_s: u64) -> Result<Vec<Value>> {
    let started = Instant::now();
    loop {
        let mut running = false;
        for c in copies.iter_mut() {
            if c.child.try_wait()?.is_none() {
                running = true;
                if let Some(s) = sample(&c.child) {
                    c.samples.push(s);
                }
            }
        }
        if !running {
            break;
        }
        if started.elapsed() > Duration::from_secs(limit_s + 30) {
            for c in copies.iter_mut() {
                let _ = c.child.kill();
            }
            fail!("os apps não terminaram a tempo");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    copies
        .iter()
        .map(|c| {
            let path = dir.join(format!("{}.json", c.who));
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let report: Value = serde_json::from_str(&text)?;
            if let Some(problem) = report["problem"].as_str().filter(|p| !p.is_empty()) {
                println!("    aviso: o app de {} parou com \"{problem}\"", c.who);
            }
            Ok(report)
        })
        .collect()
}

/// The processor time a process used, from the cycles its threads ran, at
/// the rate of the processor's time stamp counter. Windows' own count
/// (GetProcessTimes) goes by the timer's 15.6 ms tick and misses short
/// bursts: measured with a test program, 0.3 ms twenty times a second
/// (0.6 %) read as 0.0 %, and thirty times a second (0.9 %) as 1.1 to
/// 1.9 %; the cycles gave 0.61 and 0.91 %.
fn cycles_ms(handle: std::os::windows::io::RawHandle) -> Option<f64> {
    use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;
    let hz = tsc_hz()?;
    let mut cycles = 0u64;
    // SAFETY: the handle is the child's, open while it lives; the pointer
    // is to a local.
    let ok = unsafe { QueryProcessCycleTime(handle, &mut cycles) };
    (ok != 0).then(|| cycles as f64 / hz * 1000.0)
}

/// The time stamp counter's rate, measured once against the clock.
fn tsc_hz() -> Option<f64> {
    static HZ: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    *HZ.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: reading the counter has no preconditions on x86_64.
            let read = || unsafe { core::arch::x86_64::_rdtsc() };
            let (t0, c0) = (Instant::now(), read());
            std::thread::sleep(Duration::from_millis(250));
            let (t1, c1) = (Instant::now(), read());
            let hz = (c1 - c0) as f64 / (t1 - t0).as_secs_f64();
            (hz > 1e8).then_some(hz)
        }
        #[cfg(not(target_arch = "x86_64"))]
        None
    })
}

fn sample(child: &Child) -> Option<Sample> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
    };
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let handle = child.as_raw_handle();
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the handle belongs to `child` and stays open while it lives;
    // the four pointers are to locals.
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return None;
    }
    // SAFETY: a plain struct of integers, for which all zeros is valid.
    let mut mem: PROCESS_MEMORY_COUNTERS_EX2 = unsafe { std::mem::zeroed() };
    mem.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
    // SAFETY: as above, and `cb` gives the size of the struct written to.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            handle,
            std::ptr::from_mut(&mut mem).cast::<PROCESS_MEMORY_COUNTERS>(),
            mem.cb,
        )
    };
    if ok == 0 {
        return None;
    }
    let ms = |t: FILETIME| {
        ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 10_000.0
    };
    const MB: f64 = 1024.0 * 1024.0;
    Some(Sample {
        at: wall_ms(),
        cpu_ms: cycles_ms(handle).unwrap_or(ms(kernel) + ms(user)),
        private_mb: mem.PrivateUsage as f64 / MB,
        working_mb: mem.WorkingSetSize as f64 / MB,
        own_mb: mem.PrivateWorkingSetSize as f64 / MB,
    })
}

/// The processor (percent of one core) and memory (MB: private on average,
/// in RAM at most, in RAM and its own at most) of a copy between two of its
/// marks.
fn usage(c: &Copy, report: &Value, from: &str, to: &str) -> Option<(f64, f64, f64, f64)> {
    let (a, b) = (
        report["marks"][from].as_f64()?,
        report["marks"][to].as_f64()?,
    );
    let inside: Vec<&Sample> = c
        .samples
        .iter()
        .filter(|s| s.at >= a && s.at <= b)
        .collect();
    let (first, last) = (inside.first()?, inside.last()?);
    let span = last.at - first.at;
    if span < 1000.0 {
        return None;
    }
    Some((
        (last.cpu_ms - first.cpu_ms) / span * 100.0,
        inside.iter().map(|s| s.private_mb).sum::<f64>() / inside.len() as f64,
        inside.iter().map(|s| s.working_mb).fold(0.0, f64::max),
        inside.iter().map(|s| s.own_mb).fold(0.0, f64::max),
    ))
}

fn usage_lines(
    out: &mut Numbers,
    part: &str,
    copies: &[Copy],
    reports: &[Value],
    phases: &[(&str, &str)],
) {
    for (c, report) in copies.iter().zip(reports) {
        for (from, to) in phases {
            let Some((cpu, private, working, own)) = usage(c, report, from, to) else {
                println!("    {} ({from}): sem amostras de uso", c.who);
                continue;
            };
            println!(
                "    {} ({from}): processador {cpu:.1} % de um núcleo · memória {private:.0} MB \
                 (na RAM até {working:.0} MB, só dele {own:.0} MB)",
                c.who
            );
            let key = format!("{part}.{from}.{}", c.who);
            out.insert(format!("{key}.cpu"), cpu);
            out.insert(format!("{key}.ram"), private);
            out.insert(format!("{key}.ram_max"), working);
            out.insert(format!("{key}.ram_propria"), own);
        }
        // Where the memory was at the end (the app's own count).
        let m = &report["memoria"];
        if let Some(private) = m["privada"].as_f64() {
            let heaps = m["heaps_usado"].as_f64().unwrap_or(0.0);
            let held = m["heaps_retido"].as_f64().unwrap_or(0.0);
            println!(
                "    {} (no fim): privada {private:.0} MB · alocado nos heaps {heaps:.0} MB \
                 (retidos {held:.0}) · fora dos heaps {:.0} MB (gpu {:.0}) · imagens {:.0} MB",
                c.who,
                (private - held).max(0.0),
                m["gpu"].as_f64().unwrap_or(0.0),
                m["imagens"].as_f64().unwrap_or(0.0),
            );
            let key = format!("{part}.fim.{}", c.who);
            out.insert(format!("{key}.heaps"), heaps);
            out.insert(format!("{key}.fora_dos_heaps"), (private - held).max(0.0));
        }
    }
}

// ---------------------------------------------------------------- numbers

/// A series of the app's own measurements, from its report.
fn find(report: &Value, series: &str) -> Option<Value> {
    report["medidas"]["series"].get(series).cloned()
}

/// "ini:msg.http" -> "http".
fn short(series: &str) -> &str {
    let name = series.rsplit(':').next().unwrap_or(series);
    name.rsplit('.').next().unwrap_or(name)
}

fn stats(v: &[f64]) -> Option<Value> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let pick = |q: f64| s[((s.len() - 1) as f64 * q).round() as usize];
    Some(json!({
        "n": s.len(),
        "p50": pick(0.5),
        "p95": pick(0.95),
        "max": s[s.len() - 1],
    }))
}

fn num(x: f64) -> String {
    if x.abs() < 10.0 {
        format!("{x:.1}")
    } else {
        format!("{x:.0}")
    }
}

fn print_line(label: &str, s: &Option<Value>) {
    match s {
        Some(v) => println!(
            "    {label}: p50 {} ms · p95 {} ms (n={})",
            num(v["p50"].as_f64().unwrap_or(0.0)),
            num(v["p95"].as_f64().unwrap_or(0.0)),
            v["n"]
        ),
        None => println!("    {label}: sem medidas"),
    }
}

fn keep(out: &mut Numbers, key: &str, s: &Option<Value>) {
    if let Some(v) = s {
        for q in ["p50", "p95"] {
            if let Some(x) = v[q].as_f64() {
                out.insert(format!("{key}.{q}"), x);
            }
        }
    }
}

/// A file's worth of bytes that do not compress (as a video would not).
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Keeps the numbers in build/medidas/<label>.json and, with `compare`,
/// shows them next to an earlier run's.
fn save(label: Option<String>, compare: Option<&str>, numbers: &Numbers) -> Result {
    let folder = root().join("build").join("medidas");
    std::fs::create_dir_all(&folder)?;
    let label = label.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string());
    let path = folder.join(format!("{label}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(numbers)?)?;
    println!("==> Guardado em {}", path.display());
    let Some(other) = compare else {
        return Ok(());
    };
    let before_path = folder.join(format!("{other}.json"));
    let before: Numbers = serde_json::from_slice(
        &std::fs::read(&before_path).map_err(|e| format!("{}: {e}", before_path.display()))?,
    )?;
    println!("==> Comparado com {other}");
    for (key, now) in numbers {
        if let Some(was) = before.get(key) {
            let change = if *was == 0.0 {
                String::new()
            } else {
                format!(" ({:+.0} %)", (now - was) / was * 100.0)
            };
            println!("    {key}: {} → {}{change}", num(*was), num(*now));
        }
    }
    Ok(())
}
