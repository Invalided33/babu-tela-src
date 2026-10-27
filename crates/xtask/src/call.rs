//! `cargo xtask chamada`: a real call between copies of the app on this PC,
//! through the server on Cloudflare (in a test database of its own). Calls
//! are peer to peer: every pair of people has a connection of its own.
//!
//!   cargo xtask chamada              duas pessoas
//!   cargo xtask chamada --pessoas 4  quatro pessoas (até 6)
//!   cargo xtask chamada --calado     a última pessoa fica em silêncio: com
//!                                    o DTX, quase nada sai dela
//!   cargo xtask chamada --so-tcp     só pelo relé por TCP/TLS, como numa
//!                                    rede que bloqueia UDP
//!   cargo xtask chamada --tela       Ana também compartilha a tela (pela
//!                                    placa de vídeo quando dá, como no app)
//!   cargo xtask chamada --placa      como --tela, com as medições do app
//!                                    ligadas e a tela pela placa de vídeo
//!                                    (voice/hw.rs)
//!   cargo xtask chamada --janela "Título"
//!                                    Ana compartilha só essa janela
//!   cargo xtask chamada --apresentar arquivo.pptx
//!                                    Ana apresenta o arquivo (.pptx ou
//!                                    PDF) e vai para o slide 2; todos devem
//!                                    receber todos os slides
//!   cargo xtask chamada --tela --anotar (ou --apresentar ARQUIVO --anotar)
//!                                    Ana e Beto anotam sobre a tela (ou o
//!                                    slide) da Ana, Beto também com o
//!                                    ponteiro; Caio entra depois e deve
//!                                    receber tudo da Ana; aí a Ana deixa
//!                                    só ela anotar, desfaz o seu traço e o
//!                                    Beto limpa os dele
//!   cargo xtask chamada --segundos 30
//!   cargo xtask chamada --exe target\release\babutela.exe
//!                                    com um app já compilado (o que vai no
//!                                    instalador), sem compilar o de teste
//!
//! Each copy runs the app's own test mode (--smoke-report): it signs in,
//! joins the voice channel, stays for a while and writes down what it saw
//! (packets from each person, how each connection goes, which copy of
//! whose video) and a picture of its window. Instead of the microphone,
//! each one "speaks" a test voice (two seconds of talking, one of silence),
//! so the call sounds the same whatever the room does.
//!
//! With a shared screen, the people watching do different things: Beto
//! enlarges it (and should get the big copy), Caio goes to a text channel
//! (and should get nothing), the rest keep the tiles (the small copy).
//!
//! The PC's sound goes with Ana's screen: a quiet tone that a player of the
//! test's own plays (Ana takes only that player's sound, or each copy would
//! take the others' calls). Everyone should hear it but Caio, who silenced
//! it on his side first, so Ana should not even send it to him.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::e2e::T;
use crate::tools::{self, block_on, escape, root};
use crate::{Result, cf, fail};

/// User name and display name of each person in the call.
const PEOPLE: [(&str, &str); 6] = [
    ("ana", "Ana"),
    ("beto", "Beto"),
    ("caio", "Caio"),
    ("dani", "Dani"),
    ("eva", "Eva"),
    ("fabio", "Fábio"),
];

/// What each person does while Ana shares her screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Watch {
    /// Shares (Ana).
    Shares,
    /// Enlarges Ana's video: the big copy.
    Enlarges,
    /// Opens a text channel: no video at all.
    Leaves,
    /// Keeps the tiles: the small copy.
    Tiles,
}

impl Watch {
    fn of(i: usize) -> Watch {
        match i {
            0 => Watch::Shares,
            1 => Watch::Enlarges,
            2 => Watch::Leaves,
            _ => Watch::Tiles,
        }
    }

    /// The copy of Ana's video this person should be getting.
    fn layer(self) -> &'static str {
        match self {
            Watch::Shares => "",
            Watch::Enlarges => "high",
            Watch::Leaves => "off",
            Watch::Tiles => "low",
        }
    }
}

pub fn run(args: &[String]) -> Result {
    let mut tcp_only = false;
    let mut quiet_last = false;
    // What Ana shares: "screen" (the main one) or "window:<title>".
    let mut share: Option<String> = None;
    // The app's measuring mode (`--placa`), in which a screen goes the way
    // the step names (`screen-zero`: on the graphics card).
    let mut measuring = false;
    // An app already built (the release's, to test what goes out).
    let mut given_exe: Option<PathBuf> = None;
    // A file Ana presents (slides/ in the app).
    let mut present: Option<PathBuf> = None;
    let mut seconds = 20u64;
    let mut people = 2usize;
    // Annotating over what Ana shares (the app's ink/).
    let mut annotate = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--anotar" => annotate = true,
            "--so-tcp" => tcp_only = true,
            "--calado" => quiet_last = true,
            "--tela" => share = Some("screen".into()),
            "--placa" => {
                share = Some("screen-zero".into());
                measuring = true;
            }
            "--exe" => {
                let path = rest.next().ok_or("--exe quer o caminho do app")?;
                given_exe = Some(PathBuf::from(path));
            }
            "--janela" => {
                let title = rest.next().ok_or("--janela quer o título da janela")?;
                share = Some(format!("window:{title}"));
            }
            "--apresentar" => {
                let file = rest.next().ok_or("--apresentar quer o arquivo")?;
                let file = std::path::absolute(file)?;
                if !file.is_file() {
                    fail!("não achei {}", file.display());
                }
                present = Some(file);
            }
            "--segundos" => {
                seconds = rest
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|s| (10..=90).contains(s))
                    .ok_or("--segundos quer um número de 10 a 90")?;
            }
            "--pessoas" => {
                people = rest
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|n| (2..=PEOPLE.len()).contains(n))
                    .ok_or("--pessoas quer um número de 2 a 6")?;
            }
            other => fail!("opção desconhecida: {other}"),
        }
    }
    if quiet_last && share.is_some() && people < 3 {
        fail!("--calado com --tela precisa de pelo menos 3 pessoas");
    }
    if present.is_some() && share.is_some() {
        fail!("--apresentar não vai junto com --tela, --placa ou --janela");
    }
    if annotate && share.is_none() && present.is_none() {
        fail!("--anotar quer --tela, --janela ou --apresentar: é sobre o que a Ana compartilha");
    }
    if annotate && people < 3 {
        fail!("--anotar precisa de pelo menos 3 pessoas (o Caio entra depois)");
    }
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
            println!("==> Compilando o app");
            tools::run(
                tools::cargo()
                    .current_dir(root())
                    .env("PATH", tools::path_with_cmake()?)
                    .args(["build", "-p", "babutela"]),
                "o build do app",
            )?;
            root().join("target").join("debug").join("babutela.exe")
        }
    };

    println!("==> Preparando a conversa num banco de teste");
    let who = &PEOPLE[..people];
    let (header, run, tokens) = block_on(async {
        let t = T::new(cfg.endereco.clone(), &secret)?;
        let mut tokens = Vec::new();
        for (name, display) in who {
            tokens.push(t.session(name, display).await?);
        }
        let sid = t
            .post(
                &tokens[0],
                "/servers",
                json!({ "name": "Chamada de teste" }),
            )
            .await?
            .id("criar servidor")?;
        let code = t
            .post(&tokens[0], &format!("/servers/{sid}/invites"), Value::Null)
            .await?
            .ok("convite")?["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        for token in &tokens[1..] {
            t.post(
                token,
                &format!("/invites/{}/join", escape(&code)),
                Value::Null,
            )
            .await?
            .ok("entrar")?;
        }
        Ok::<_, Box<dyn std::error::Error>>((t.header.clone(), t.run.clone(), tokens))
    })??;

    let dir = std::env::temp_dir().join(format!("babutela-chamada-{run}"));
    std::fs::create_dir_all(&dir)?;
    // The sound of Ana's screen: a player of its own, for the whole call.
    let tone = match &share {
        Some(_) => Some(Tone::play(&dir, seconds + 120)?),
        None => None,
    };
    let quiet = |i: usize| quiet_last && i == people - 1;
    let names: Vec<&str> = who.iter().map(|(_, display)| *display).collect();
    let how = if tcp_only {
        "só pelo relé TCP/TLS"
    } else {
        "direto entre as pessoas"
    };
    println!(
        "==> {} entram na sala de voz ({how}, {seconds} s)",
        names.join(", ")
    );
    let mut children = Vec::new();
    // Annotating, everyone takes the steps that matter at the same moments
    // of the clock (`until:`), from a little after they have all opened.
    let base = wall_ms() + 10_000.0;
    for (i, ((name, _), token)) in who.iter().zip(&tokens).enumerate() {
        // Each one arrives 2 s after the one before, and they all write
        // down how the call is at the same moment, halfway through, with
        // everyone in it ("note:meio").
        let half = seconds * 1000 / 2;
        let lead = half + 2000 * (people - 1 - i) as u64;
        let steps = match (&share, Watch::of(i)) {
            _ if annotate => annotate_steps(i, &share, present.as_deref(), base),
            // Ana presents, and a moment later goes to slide 2. The two
            // steps take 5 s (one step's time each, and 2 s for the
            // presenting), off the wait before the note.
            (None, Watch::Shares) if let Some(file) = &present => format!(
                "go:first-server|join|apresentar:{}|slide:2|wait:{}|note:meio|wait:{half}",
                file.display(),
                lead.saturating_sub(5000)
            ),
            // After the note, the others go two slides ahead on their own
            // and zoom in, so the picture at the end shows how that looks
            // (the button back to Ana's slide, the controls).
            (None, _) if present.is_some() => format!(
                "go:first-server|join|wait:{lead}|note:meio|hover:slide|tecla:ArrowRight|tecla:ArrowRight|roda:200|wait:{}",
                half.saturating_sub(6000)
            ),
            (Some(what), Watch::Shares) => {
                format!("go:first-server|join|share:{what}|wait:{lead}|note:meio|wait:{half}")
            }
            // With Ana's menu open over her screen (a right click on it)
            // from early on, so the picture at the end shows it. The step
            // takes one step's time (1.5 s) off the wait before the note.
            (Some(_), Watch::Enlarges) => format!(
                "go:first-server|join|zoom:ana|person:ana|wait:{}|note:meio|wait:{half}",
                lead.saturating_sub(1500)
            ),
            // Silences the sound of Ana's screen before the call, and
            // leaves the call view (staying in the call) well before the
            // note, so Ana has long stopped sending to them.
            (Some(_), Watch::Leaves) => format!(
                "silence:ana:screen|go:first-server|join|wait:{}|go:text|wait:{}|note:meio|wait:{half}",
                lead.saturating_sub(5000),
                lead.min(5000),
            ),
            _ => format!("go:first-server|join|wait:{lead}|note:meio|wait:{half}"),
        };
        if i > 0 {
            // A moment apart, like people arriving.
            std::thread::sleep(Duration::from_secs(2));
        }
        let voice = if quiet(i) { "silencio" } else { "fala" };
        let sound_from = tone.as_ref().filter(|_| i == 0).map(Tone::pid);
        children.push(launch(
            &exe, &dir, name, &header, token, &steps, tcp_only, voice, seconds, measuring,
            sound_from,
        )?);
    }
    let limit = Duration::from_secs(seconds + 120);
    let mut reports = Vec::new();
    for (child, (name, _)) in children.iter_mut().zip(who) {
        reports.push(wait(child, &dir, name, limit)?);
    }

    let uid = |r: &Value| r["userId"].as_i64().unwrap_or(0);
    let ids: Vec<i64> = reports.iter().map(uid).collect();
    // By the note, everyone has been in the call together for about half
    // of it. Someone speaking two seconds out of three sends about 33
    // packets a second; a third of that is plenty to say the voice got
    // through. Someone silent sends DTX's odd comfort-noise packet.
    let half = seconds / 2;
    let enough = half * 50 * 2 / 3 / 3;
    let at_most_silent = half * 50 / 8;
    let mut problems = Vec::new();
    for (i, (r, display)) in reports.iter().zip(&names).enumerate() {
        let call = &r["notes"]["meio"]["voiceCall"];
        let peers: BTreeMap<i64, &Value> = call["peers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["user"].as_i64()?, p)))
            .collect();
        let mut heard = Vec::new();
        for (j, other) in ids.iter().enumerate().filter(|(j, _)| *j != i) {
            let Some(p) = peers.get(other) else {
                problems.push(format!("{display}: sem conexão com {}", names[j]));
                continue;
            };
            let got = p["audioIn"].as_u64().unwrap_or(0);
            let route = if p["relayed"] == true {
                "relé"
            } else {
                "direto"
            };
            heard.push(format!("{} {got} ({route})", names[j]));
            if p["connected"] != true {
                problems.push(format!("{display}: a conexão com {} não abriu", names[j]));
            }
            if (p["relayed"] == true) != tcp_only {
                problems.push(format!(
                    "{display}: a conexão com {} foi {route}, esperava {}",
                    names[j],
                    if tcp_only { "pelo relé" } else { "direto" }
                ));
            }
            if quiet(j) {
                if got > at_most_silent {
                    problems.push(format!(
                        "{display} recebeu {got} pacotes de {}, que estava calada \
                         (esperava no máximo {at_most_silent})",
                        names[j]
                    ));
                }
            } else if got < enough {
                problems.push(format!(
                    "{display} recebeu {got} pacotes de voz de {} (esperava pelo menos {enough})",
                    names[j]
                ));
            }
        }
        let voice = &call["voice"];
        let noise = &r["notes"]["meio"]["noise"];
        println!(
            "    {display}: voz a {} kb/s em pacotes de {} ms, supressão de ruído {} ({:.1} ms por 10 ms); ouviu {}",
            voice["bitrate"].as_i64().unwrap_or(0) / 1000,
            voice["frameMs"],
            noise["now"].as_str().unwrap_or("nenhuma"),
            noise["msPer10ms"].as_f64().unwrap_or(0.0),
            heard.join(", ")
        );
        // The app's own choice is the network; mid-call it must be at work.
        if noise["chosen"] == "ia" && noise["now"] != "ia" {
            problems.push(format!(
                "{display}: a supressão de ruído por IA não estava ativa no meio da chamada ({noise})"
            ));
        }
        if let Some(problem) = r["problem"].as_str().filter(|p| !p.is_empty()) {
            problems.push(format!("{display}: {problem}"));
        }
    }
    if quiet_last {
        let sent = reports[people - 1]["voiceStats"][0].as_u64().unwrap_or(0);
        println!(
            "    {} (calada) mandou {sent} pacotes no total",
            names[people - 1]
        );
    }
    for (display, (name, _)) in names.iter().zip(who) {
        println!(
            "    {display}: {}",
            dir.join(format!("{name}.png")).display()
        );
    }
    // Annotating, the others do other things than watching the video's
    // copies (`Watch`).
    if share.is_some() && !annotate {
        check_video(&reports, &names, &ids, &mut problems);
        check_screen_sound(&reports, &names, &ids, seconds, &mut problems);
    }
    if present.is_some() {
        check_slides(&reports, &names, &ids, &mut problems);
    }
    if annotate {
        check_ink(&reports, &names, &ids, share.is_some(), &mut problems);
    }
    drop(tone);
    if !problems.is_empty() {
        fail!("{}", problems.join("\n"));
    }
    println!("A chamada funcionou ({how}).");
    Ok(())
}

fn wall_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// The note in the middle and the one at the end, annotating: so long
/// after `base`.
const INK_MIDDLE: f64 = 26_000.0;
const INK_END: f64 = 36_000.0;

/// What person `i` does, annotating over what Ana shares (her screen, or
/// her presentation, on slide 2). Ana draws an arrow; Beto enlarges it (or
/// watches the presentation) and draws a pen stroke, a rectangle and,
/// with the pointer, another pen stroke; Caio arrives after all that.
/// Halfway, everyone writes down what they have. Then Ana lets only
/// herself annotate and undoes her arrow, and Beto clears his: at the end
/// there is nothing on it, and only Ana may annotate.
fn annotate_steps(
    i: usize,
    share: &Option<String>,
    present: Option<&std::path::Path>,
    base: f64,
) -> String {
    let at = |ms: f64| format!("until:{:.0}", base + ms);
    let (middle, end) = (at(INK_MIDDLE), at(INK_END));
    let notes = |before: &str, after: &str| {
        format!("{before}|{middle}|note:meio|{after}|{end}|note:fim|wait:3000")
    };
    match i {
        0 => {
            let open = match (share, present) {
                (Some(what), _) => format!("share:{what}"),
                (None, Some(file)) => format!("apresentar:{}|slide:2", file.display()),
                (None, None) => String::new(),
            };
            // Her own screen enlarged too, so her picture at the end shows
            // the toolbar. Beto clears his strokes before she stops him.
            let look = if share.is_some() { "|zoom:ana" } else { "" };
            notes(
                &format!(
                    "go:first-server|join|{open}{look}|{}|anotar:ana|ferramenta:arrow|traco:0.6,0.6;0.9,0.9",
                    at(6000.0)
                ),
                "wait:2000|so-eu:on|desfazer",
            )
        }
        1 => {
            // The screen enlarged, or the presentation watched: where the
            // pointer draws.
            let look = if share.is_some() {
                "zoom:ana"
            } else {
                "click:botao:Assistir"
            };
            notes(
                &format!(
                    "go:first-server|join|{}|{look}|anotar:ana|ferramenta:pen|cor:4|traco:0.2,0.2;0.5,0.5;0.8,0.3|ferramenta:rect|traco:0.1,0.6;0.4,0.9|ferramenta:pen|arrastar:anotacao",
                    at(7000.0)
                ),
                "limpar:meus",
            )
        }
        2 => notes(
            &format!("go:first-server|{}|join", at(15_000.0)),
            "wait:1000",
        ),
        _ => notes("go:first-server|join", "wait:1000"),
    }
}

/// Annotating: halfway, everyone (Caio too, who arrived after) has Ana's
/// arrow and Beto's three strokes over what Ana shares; at the end,
/// nothing, and only Ana may annotate.
fn check_ink(
    reports: &[Value],
    names: &[&str],
    ids: &[i64],
    screen: bool,
    problems: &mut Vec<String>,
) {
    let (ana, beto) = (ids[0], ids[1]);
    // Sharing her screen, Ana sees everyone's strokes over it, in a window
    // Windows leaves out of the capture.
    if screen {
        let overlay = &reports[0]["notes"]["meio"]["ink"]["overlay"];
        println!("    Ana, a janela sobre a tela dela no meio: {overlay}");
        if overlay["excluded"] != true || overlay["shown"] != true || overlay["strokes"] != 4 {
            problems.push(format!(
                "a janela das anotações sobre a tela da Ana não estava como devia (fora da captura, à vista, com os 4 traços): {overlay}"
            ));
        }
    }
    for (r, display) in reports.iter().zip(names) {
        let surface = |note: &str| {
            r["notes"][note]["ink"]["surfaces"]
                .as_array()
                .and_then(|list| list.iter().find(|s| s["owner"].as_i64() == Some(ana)))
                .cloned()
        };
        let by = |s: &Option<Value>, uid: i64| {
            s.as_ref()
                .and_then(|s| s["strokesBy"][uid.to_string()].as_u64())
                .unwrap_or(0)
        };
        let (middle, end) = (surface("meio"), surface("fim"));
        println!(
            "    {display}: no meio, {} traço(s) da Ana e {} do Beto; no fim, {} e {} (todos podem anotar: {})",
            by(&middle, ana),
            by(&middle, beto),
            by(&end, ana),
            by(&end, beto),
            end.as_ref().map_or(Value::Null, |s| s["everyone"].clone())
        );
        if middle.is_none() {
            problems.push(format!("{display} não via nada da Ana para anotar no meio"));
            continue;
        }
        if by(&middle, ana) != 1 || by(&middle, beto) != 3 {
            problems.push(format!(
                "{display} tinha {} traço(s) da Ana e {} do Beto no meio (esperava 1 e 3; o terceiro do Beto é com o ponteiro)",
                by(&middle, ana),
                by(&middle, beto)
            ));
        }
        match &end {
            None => problems.push(format!(
                "{display} não via mais o que a Ana compartilha no fim"
            )),
            Some(s) => {
                if by(&end, ana) + by(&end, beto) != 0 {
                    problems.push(format!(
                        "{display} ainda tinha traços no fim (a Ana desfez o dela, o Beto limpou os dele): {}",
                        s["strokesBy"]
                    ));
                }
                if s["everyone"] != false {
                    problems.push(format!(
                        "{display}: a Ana deixou só ela anotar, mas ali estava {}",
                        s["everyone"]
                    ));
                }
            }
        }
    }
}

/// A quiet tone playing for as long as the call, from a process of its
/// own (PowerShell's sound player): the PC's sound for Ana's screen.
struct Tone(Child);

impl Tone {
    fn play(dir: &std::path::Path, seconds: u64) -> Result<Tone> {
        let wav = dir.join("tom.wav");
        std::fs::write(&wav, tone_wav())?;
        let script = format!(
            "$p = New-Object System.Media.SoundPlayer '{}'; $p.PlayLooping(); Start-Sleep -Seconds {seconds}",
            wav.display()
        );
        let child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .spawn()?;
        Ok(Tone(child))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One second of a quiet 440 Hz tone (a whole number of cycles, so it
/// loops without a click): 48 kHz, 16-bit, mono WAV.
fn tone_wav() -> Vec<u8> {
    const RATE: u32 = 48_000;
    let samples: Vec<i16> = (0..RATE)
        .map(|i| {
            let t = f64::from(i) / f64::from(RATE);
            (0.08 * (std::f64::consts::TAU * 440.0 * t).sin() * 32_767.0) as i16
        })
        .collect();
    let data_len = samples.len() as u32 * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// The PC's sound of Ana's screen: everyone hears it but Caio, who
/// silenced it, and to whom Ana should not be sending it at all.
fn check_screen_sound(
    reports: &[Value],
    names: &[&str],
    ids: &[i64],
    seconds: u64,
    problems: &mut Vec<String>,
) {
    let ana = &reports[0];
    if ana["sharingAudio"] != true {
        problems.push("o som do PC não ia junto com a tela da Ana".into());
    }
    let to: BTreeMap<i64, bool> = ana["notes"]["meio"]["voiceCall"]["peers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["user"].as_i64()?, p["screenAudioOut"].as_bool()?)))
        .collect();
    // A tone that never stops: 50 packets a second from when the tracks
    // are set up; a fifth of the first half is plenty to say it came.
    let enough = seconds / 2 * 50 / 5;
    for (i, (r, display)) in reports.iter().zip(names).enumerate().skip(1) {
        let from_ana = r["notes"]["meio"]["voiceCall"]["peers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["user"].as_i64() == Some(ids[0]))
            .cloned()
            .unwrap_or_default();
        let got = from_ana["screenAudioIn"].as_u64().unwrap_or(0);
        let sending = to.get(&ids[i]).copied().unwrap_or(false);
        let silenced = Watch::of(i) == Watch::Leaves;
        if Watch::of(i) == Watch::Enlarges {
            let menu = format!("Person({})", ids[0]);
            if r["pop"] != menu.as_str() {
                problems.push(format!(
                    "{display} não ficou com o menu da Ana aberto: {}",
                    r["pop"]
                ));
            }
        }
        println!(
            "    {display}: {got} pacotes do som da tela da Ana{}",
            if silenced {
                " (silenciado por ele)"
            } else {
                ""
            }
        );
        if silenced {
            if got > 0 || sending {
                problems.push(format!(
                    "{display} silenciou o som da tela da Ana, mas recebeu {got} pacotes"
                ));
            }
        } else if got < enough {
            problems.push(format!(
                "{display} recebeu {got} pacotes do som da tela da Ana (esperava pelo menos {enough})"
            ));
        }
    }
}

/// Ana presents a file: by the note everyone should have every slide of
/// it, on the one she went to, and she should count them as sent.
fn check_slides(reports: &[Value], names: &[&str], ids: &[i64], problems: &mut Vec<String>) {
    let mine = &reports[0]["notes"]["meio"]["slides"]["mine"];
    let count = mine["count"].as_u64().unwrap_or(0);
    let slide = |v: &Value| v["at"].as_u64().map_or(0, |at| at + 1);
    println!(
        "    Ana apresentou {}: {count} slides, {} desenhados ({} KB), no slide {}",
        mine["name"],
        mine["drawn"],
        mine["bytes"].as_u64().unwrap_or(0) / 1024,
        slide(mine)
    );
    if count == 0 {
        problems.push(format!("a Ana não estava apresentando: {mine}"));
        return;
    }
    if mine["drawn"].as_u64() != Some(count) {
        problems.push(format!(
            "a Ana desenhou {} de {count} slides",
            mine["drawn"]
        ));
    }
    if mine["failed"].as_u64().unwrap_or(0) > 0 {
        problems.push(format!("{} slides não saíram", mine["failed"]));
    }
    for (i, (r, display)) in reports.iter().zip(names).enumerate().skip(1) {
        let theirs = r["notes"]["meio"]["slides"]["theirs"]
            .as_array()
            .and_then(|list| list.iter().find(|d| d["from"].as_i64() == Some(ids[0])))
            .cloned()
            .unwrap_or_default();
        let received = theirs["received"].as_u64().unwrap_or(0);
        let sent = mine["sentTo"][ids[i].to_string()].as_u64().unwrap_or(0);
        println!(
            "    {display}: recebeu {received} de {} slides ({} KB), no slide {}; a Ana contou {sent} enviados",
            theirs["count"],
            theirs["bytes"].as_u64().unwrap_or(0) / 1024,
            slide(&theirs)
        );
        if theirs["count"].as_u64() != Some(count) || received != count {
            problems.push(format!(
                "{display} recebeu {received} de {count} slides da Ana"
            ));
        }
        if slide(&theirs) != slide(mine) {
            problems.push(format!(
                "{display} estava no slide {}, a Ana no {}",
                slide(&theirs),
                slide(mine)
            ));
        }
        if sent != count {
            problems.push(format!(
                "a Ana contou {sent} de {count} slides enviados para {display}"
            ));
        }
    }
}

/// Ana shares: everyone should get the copy that fits what they do, and
/// Ana should be sending exactly that.
fn check_video(reports: &[Value], names: &[&str], ids: &[i64], problems: &mut Vec<String>) {
    let ana = &reports[0];
    let call = &ana["notes"]["meio"]["voiceCall"];
    println!(
        "    Ana compartilhou {}: orçamento {} kb/s (cópia pequena {} kb/s, grande {} kb/s)",
        ana["sharingSource"], call["budgetKbps"], call["lowKbps"], call["highKbps"]
    );
    if ana["sharing"] != "screen" {
        problems.push(format!(
            "a Ana não estava compartilhando: {}",
            ana["sharing"]
        ));
    }
    let sent: BTreeMap<i64, String> = call["peers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["user"].as_i64()?, p["sending"].as_str()?.to_owned())))
        .collect();
    for (i, (r, display)) in reports.iter().zip(names).enumerate().skip(1) {
        let watch = Watch::of(i);
        let going = sent.get(&ids[i]).map_or("?", String::as_str);
        let them = &r["notes"]["meio"]["voiceCall"]["peers"];
        let from_ana = them
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["user"].as_i64() == Some(ids[0]))
            .cloned()
            .unwrap_or_default();
        let size = &from_ana["videoSize"];
        let width = size[0].as_u64().unwrap_or(0);
        println!(
            "    {display} ({watch:?}): pediu {}, a Ana mandava {going}; {} imagens, a última {}x{}",
            from_ana["showing"],
            from_ana["videoIn"],
            width,
            size[1].as_u64().unwrap_or(0),
        );
        if going != watch.layer() {
            problems.push(format!(
                "a Ana mandava \"{going}\" para {display}, esperava \"{}\"",
                watch.layer()
            ));
        }
        let pictures = from_ana["videoIn"].as_u64().unwrap_or(0);
        match watch {
            Watch::Enlarges if width <= 640 => problems.push(format!(
                "{display} ampliou o vídeo mas recebeu {width} de largura (a cópia pequena)"
            )),
            Watch::Tiles if width == 0 || width > 640 => problems.push(format!(
                "{display} deveria receber a cópia pequena, recebeu {width} de largura"
            )),
            Watch::Enlarges | Watch::Tiles if pictures == 0 => {
                problems.push(format!("{display} não recebeu a imagem da Ana"));
            }
            _ => {}
        }
        // Who stays in the call view should still have her picture at the end.
        if watch != Watch::Leaves {
            let sees = r["videoFrom"]
                .as_array()
                .is_some_and(|from| from.iter().any(|id| id.as_i64() == Some(ids[0])));
            if !sees {
                problems.push(format!(
                    "{display} não tinha a imagem da Ana no fim: {}",
                    r["videoFrom"]
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn launch(
    exe: &PathBuf,
    dir: &std::path::Path,
    who: &str,
    header: &str,
    token: &str,
    steps: &str,
    tcp_only: bool,
    voice: &str,
    seconds: u64,
    measuring: bool,
    sound_from: Option<u32>,
) -> Result<Child> {
    let mut cmd = Command::new(exe);
    if let Some(pid) = sound_from {
        // The app's voice/loopback.rs: take this process's sound.
        cmd.env("BABUTELA_TESTE_SOM_DE", pid.to_string());
    }
    cmd.env("PROSA_USERDATA", dir.join(format!("perfil-{who}")))
        .env("PROSA_NO_TRAY", "1")
        .env("BABUTELA_TEST_HUB", header)
        .env("PROSA_SMOKE_TOKEN", token)
        .env("PROSA_SMOKE_ACTIONS", steps)
        .env("PROSA_SMOKE_SECONDS", (seconds + 80).to_string())
        .env("BABUTELA_TESTE_VOZ", voice)
        .arg(format!(
            "--smoke-report={}",
            dir.join(format!("{who}.json")).display()
        ));
    if tcp_only {
        cmd.env("BABUTELA_TURN_SO_TCP", "1");
    }
    if measuring {
        cmd.env("BABUTELA_MEDIR", "1");
    }
    Ok(cmd.spawn()?)
}

pub(crate) fn wait(
    child: &mut Child,
    dir: &std::path::Path,
    who: &str,
    limit: Duration,
) -> Result<Value> {
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            fail!("o app de {who} não terminou a tempo");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let report = dir.join(format!("{who}.json"));
    let text =
        std::fs::read_to_string(&report).map_err(|e| format!("{}: {e}", report.display()))?;
    Ok(serde_json::from_str(&text)?)
}
