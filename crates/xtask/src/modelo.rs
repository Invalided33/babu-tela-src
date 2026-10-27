//! `cargo xtask modelo [--atualizar]`: whether Ceva published a new
//! DPDFNet, the network of the calls' noise suppression (crates/denoise),
//! and, with --atualizar, bringing it into the app.
//!
//! The app carries one of Ceva's files from Hugging Face (`REPO`), pinned:
//! crates/denoise/model/NOTICE says from which revision, with the
//! original's SHA-256. Nothing follows Ceva's newest on its own: the code
//! is built around this model (481 bins, its memory as its metadata lays
//! it out, 50 ms of look-ahead), and the README's numbers were measured
//! with it. So this says when Hugging Face has another file (`release
//! update` asks too), and --atualizar brings one in only after it does
//! what the app's does: downloaded at that revision and its SHA-256
//! checked, its weights put in float16 (onnx.rs), compared with the
//! original and run through the denoise crate's tests, before and after it
//! takes the old one's place. The NOTICE is then written again.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::tools::{self, block_on, root};
use crate::{Result, fail, onnx};

/// Ceva's models on Hugging Face, and the one the app uses.
const REPO: &str = "Ceva-IP/DPDFNet";
const FILE: &str = "onnx/dpdfnet2_48khz_hr.onnx";
const API: &str = "https://huggingface.co/api/models";

/// The app's copy, its weights in float16, and the notice next to it.
const SHIPPED: &str = "dpdfnet2_48khz_hr_f16.onnx";
const NOTICE: &str = "NOTICE";

/// Where the denoise crate's `a_new_model_matches_its_original` reads a
/// new model from (`original.onnx` and `f16.onnx`), and the file it leaves
/// there once the model passed, with how far the float16 copy came out
/// from the original.
const CHECK_ENV: &str = "BABUTELA_MODELO_NOVO";
const PASSED: &str = "conferido.txt";

fn model_dir() -> PathBuf {
    root().join("crates").join("denoise").join("model")
}

pub fn run(args: &[String]) -> Result {
    let mut update = false;
    for arg in args {
        match arg.as_str() {
            "--atualizar" => update = true,
            other => fail!("opção desconhecida: {other}"),
        }
    }
    let (pin, standing) = standing()?;
    match standing {
        Standing::Current(up) => {
            println!("==> Modelo de ruído (DPDFNet): em dia. {}", up.same());
            Ok(())
        }
        Standing::Gone if update => fail!("{}", gone()),
        Standing::Gone => {
            println!("==> Modelo de ruído (DPDFNet): {}", gone());
            Ok(())
        }
        Standing::Newer(up) => {
            println!(
                "==> Modelo de ruído (DPDFNet): a Ceva publicou um novo. {}",
                up.changed(&pin)
            );
            if update {
                bring(&up)
            } else {
                println!("    Para trazer ele para o app: cargo xtask modelo --atualizar");
                Ok(())
            }
        }
    }
}

/// For `release update`, before it builds: a word when Ceva published
/// another model than the app's. It never stops a release.
pub fn remind() {
    match standing() {
        Ok((_, Standing::Current(_))) => {
            println!("==> Modelo de ruído: o mesmo que a Ceva tem no Hugging Face");
        }
        Ok((pin, Standing::Newer(up))) => println!(
            "ATENÇÃO: a Ceva publicou um modelo de ruído novo. {} Esta versão sai com o de \
             antes; para trazer o novo: cargo xtask modelo --atualizar",
            up.changed(&pin)
        ),
        Ok((_, Standing::Gone)) => println!("ATENÇÃO: {}", gone()),
        Err(e) => println!("    (não deu para conferir o modelo de ruído no Hugging Face: {e})"),
    }
}

/// How the app's model stands against Ceva's.
enum Standing {
    /// The same file.
    Current(Upstream),
    /// Another file than the one the app's came from.
    Newer(Upstream),
    /// Ceva's repository no longer has it.
    Gone,
}

/// Where the app's model came from, as the NOTICE says.
struct Pin {
    revision: String,
    /// The original's, before its weights went to float16.
    sha256: String,
}

/// The file on Hugging Face now.
struct Upstream {
    /// The repository's revision.
    revision: String,
    sha256: String,
    size: u64,
    /// The commit that last changed the file: its id, title and date.
    commit: (String, String, String),
}

impl Upstream {
    fn same(&self) -> String {
        let (_, title, date) = &self.commit;
        format!(
            "O {FILE} do Hugging Face é o mesmo do app (mudou pela última vez em {}: \"{title}\").",
            day(date)
        )
    }

    fn changed(&self, pin: &Pin) -> String {
        let (id, title, date) = &self.commit;
        format!(
            "O do app veio da revisão {}; o {FILE} mudou em {} (\"{title}\", commit {}).",
            short(&pin.revision),
            day(date),
            short(id)
        )
    }
}

fn gone() -> String {
    format!(
        "o Hugging Face não tem mais {FILE} em {REPO}. O app continua com o que tem; veja o que \
         a Ceva mudou em https://huggingface.co/{REPO}"
    )
}

fn standing() -> Result<(Pin, Standing)> {
    let pin = pinned()?;
    let standing = match block_on(upstream())?? {
        None => Standing::Gone,
        Some(up) if up.sha256 == pin.sha256 => Standing::Current(up),
        Some(up) => Standing::Newer(up),
    };
    Ok((pin, standing))
}

/// The revision and the SHA-256 the NOTICE names.
fn pinned() -> Result<Pin> {
    let path = model_dir().join(NOTICE);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    match pin_in(&text) {
        Some(pin) => Ok(pin),
        None => fail!(
            "{} não diz de que revisão o modelo veio, nem o SHA-256 dele",
            path.display()
        ),
    }
}

/// What `notice` writes: "(revision <40 hex digits>, SHA-256 <64 more>)".
fn pin_in(text: &str) -> Option<Pin> {
    let after = |label: &str| text.split_once(label).map(|(_, rest)| rest.trim_start());
    let hex = |rest: &str, n: usize| {
        let digits: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
        (digits.len() == n).then_some(digits)
    };
    Some(Pin {
        revision: hex(after("(revision ")?, 40)?,
        sha256: hex(after("SHA-256")?, 64)?,
    })
}

/// The file in the repository's newest revision; None when it has none.
async fn upstream() -> Result<Option<Upstream>> {
    let http = tools::http()?;
    let Some(repo) = json(&http, &format!("{API}/{REPO}")).await? else {
        return Ok(None);
    };
    let Some(revision) = repo["sha"].as_str().filter(|s| is_hex(s, 40)) else {
        fail!("o Hugging Face não disse a revisão de {REPO}");
    };
    let folder = FILE.rsplit_once('/').map_or("", |(folder, _)| folder);
    let url = format!("{API}/{REPO}/tree/{revision}/{folder}?expand=true");
    let Some(tree) = json(&http, &url).await? else {
        return Ok(None);
    };
    let Some(file) = tree
        .as_array()
        .and_then(|files| files.iter().find(|f| f["path"] == FILE))
    else {
        return Ok(None);
    };
    let Some(sha256) = file["lfs"]["oid"].as_str().filter(|s| is_hex(s, 64)) else {
        fail!("o Hugging Face não disse o SHA-256 de {FILE}");
    };
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let last = &file["lastCommit"];
    Ok(Some(Upstream {
        revision: revision.to_owned(),
        sha256: sha256.to_owned(),
        size: file["lfs"]["size"].as_u64().unwrap_or(0),
        commit: (text(&last["id"]), text(&last["title"]), text(&last["date"])),
    }))
}

/// An answer of Hugging Face's API; None when it says there is no such
/// thing.
async fn json(http: &reqwest::Client, url: &str) -> Result<Option<Value>> {
    let res = http
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    if res.status().as_u16() == 404 {
        return Ok(None);
    }
    if !res.status().is_success() {
        fail!("{url} respondeu {}", res.status());
    }
    Ok(Some(res.json().await?))
}

/// Ceva's new model in the app's place, once it passed; otherwise the
/// app's stays as it was.
fn bring(up: &Upstream) -> Result {
    println!("==> Baixando {FILE} (revisão {})", short(&up.revision));
    let original = block_on(download(up))??;
    let converted = onnx::to_f16(&original)?;
    println!(
        "    SHA-256 conferido; {} pesos em float16: {} em vez de {}",
        converted.tensors,
        megabytes(converted.bytes.len()),
        megabytes(original.len())
    );

    // Checked away from the app's copy first.
    let scratch = root().join("target").join("modelo");
    std::fs::create_dir_all(&scratch)?;
    std::fs::write(scratch.join("original.onnx"), &original)?;
    std::fs::write(scratch.join("f16.onnx"), &converted.bytes)?;
    let passed = scratch.join(PASSED);
    let _ = std::fs::remove_file(&passed);
    println!("==> Conferindo o modelo novo: contra o original e nos testes do denoise");
    tools::run(
        tools::cargo()
            .current_dir(root())
            .env(CHECK_ENV, &scratch)
            .args([
                "test",
                "-p",
                "babutela-denoise",
                "--",
                "--ignored",
                "--exact",
                "tests::a_new_model_matches_its_original",
                "--nocapture",
            ]),
        "a conferência do modelo novo",
    )?;
    // A test that ran no test at all would pass too.
    let Ok(apart) = std::fs::read_to_string(&passed) else {
        fail!("a conferência do modelo novo não rodou (o teste mudou de nome?)");
    };

    let dir = model_dir();
    let (model, notice_path) = (dir.join(SHIPPED), dir.join(NOTICE));
    let before = (std::fs::read(&model)?, std::fs::read(&notice_path)?);
    let mut text = notice(
        &up.revision,
        &up.sha256,
        converted.tensors,
        original.len(),
        converted.bytes.len(),
    );
    // The line ends it had on this PC.
    if before.1.windows(2).any(|w| w == b"\r\n") {
        text = text.replace('\n', "\r\n");
    }
    std::fs::write(&model, &converted.bytes)?;
    std::fs::write(&notice_path, text)?;
    println!("==> Os testes do denoise, com o modelo novo no lugar");
    let tests = tools::run(
        tools::cargo()
            .current_dir(root())
            .args(["test", "-p", "babutela-denoise"]),
        "os testes do denoise",
    );
    if let Err(e) = tests {
        std::fs::write(&model, &before.0)?;
        std::fs::write(&notice_path, &before.1)?;
        fail!("{e}. O modelo de antes voltou para o lugar.");
    }
    let _ = std::fs::remove_dir_all(&scratch);
    println!(
        "Modelo novo no app (revisão {}; o float16 fica {} dB abaixo do som longe do original): \
         crates/denoise/model/{SHIPPED} e o NOTICE. Antes de lançar, meça a voz de novo (cargo \
         xtask medir voz, e a fala de verdade: veja \"Testes\" no README) e atualize os números \
         da supressão de ruído no README.",
        short(&up.revision),
        apart.trim()
    );
    Ok(())
}

/// The original at the revision `up` names, checked against what Hugging
/// Face says of it.
async fn download(up: &Upstream) -> Result<Vec<u8>> {
    let url = format!(
        "https://huggingface.co/{REPO}/resolve/{}/{FILE}",
        up.revision
    );
    let res = tools::http()?
        .get(&url)
        .timeout(Duration::from_secs(300))
        .send()
        .await?;
    if !res.status().is_success() {
        fail!("{url} respondeu {}", res.status());
    }
    let bytes = res.bytes().await?.to_vec();
    let sha256 = crate::sigv4::sha256_hex(&bytes);
    if sha256 != up.sha256 || (up.size != 0 && bytes.len() as u64 != up.size) {
        fail!(
            "o {FILE} baixado não confere com o que o Hugging Face diz dele ({} bytes, SHA-256 \
             {sha256})",
            bytes.len()
        );
    }
    Ok(bytes)
}

/// The NOTICE next to the app's copy: where it comes from and what changed
/// in it, as the Apache License asks of a changed file. `pin_in` reads the
/// revision and the SHA-256 back.
fn notice(revision: &str, sha256: &str, tensors: usize, original: usize, f16: usize) -> String {
    format!(
        "\
DPDFNet, the noise suppression of Babu Tela's calls

{SHIPPED} is the 48 kHz DPDFNet-2 model by Ceva
(Daniel Rika, Nino Sapir and Ido Gus), released under the Apache License 2.0
(LICENSE, next to this file).

  Paper:  \"DPDFNet: Boosting DeepFilterNet2 via Dual-Path RNN\",
          https://arxiv.org/abs/2512.16420
  Code:   https://github.com/ceva-ip/DPDFNet
  Model:  https://huggingface.co/{REPO}, {FILE}
          (revision {revision}, SHA-256
          {sha256})

Changed from the original: each of its {tensors} weight tensors with {min} values or
more is stored as float16, behind a Cast node that turns it back into
float32 when the model loads. The graph and its float32 arithmetic are
otherwise the same; the file is half the size ({f16} instead of {original})
and what it outputs stays more than 60 dB below the signal away from the
original's.
",
        min = onnx::MIN_VALUES,
        f16 = megabytes(f16),
        original = megabytes(original),
    )
}

/// Megabytes as the NOTICE gives them (of a million bytes).
fn megabytes(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// "17/05/2026" for Hugging Face's "2026-05-17T09:48:24.000Z".
fn day(date: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(date)
        .map(|d| d.format("%d/%m/%Y").to_string())
        .unwrap_or_else(|_| date.to_owned())
}

fn short(id: &str) -> &str {
    id.get(..7).unwrap_or(id)
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}
