//! `cargo xtask servidor`: builds the server (crates/worker) for WebAssembly
//! and publishes it on Cloudflare, where it keeps the database (SQLite inside
//! the Durable Object), the uploads (R2) and the call relay (TURN).

use std::process::Command;
use std::time::Duration;

use babutela_buildinfo::VERSION;

use crate::cf::{self, Cf};
use crate::tools::{self, block_on, root};
use crate::{Result, fail};

/// The same version as the `worker` crate, which checks it at build time.
const WORKER_BUILD: &str = "0.8.7";

/// Secrets of features that are gone: never sent, and taken off the Worker.
const OBSOLETE_SECRETS: [&str; 2] = ["REALTIME_APP_ID", "REALTIME_APP_SECRET"];

pub fn run(args: &[String]) -> Result {
    if let Some(extra) = args.first() {
        fail!("opção desconhecida: {extra}");
    }
    let cfg = cf::load()?;
    check(&cfg)?;
    publish_server(&cfg)
}

/// The secrets a server cannot do without.
pub fn check(cfg: &cf::Config) -> Result {
    if cfg.secret("PEPPER").is_none() {
        fail!(
            "Falta \"segredos.PEPPER\" em {}: é a chave que assina os envios de arquivos.",
            cf::FILE
        );
    }
    // The only way in. Published without them, nobody could sign in.
    for name in ["GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"] {
        if cfg.secret(name).is_none() {
            fail!(
                "Falta \"segredos.{name}\" em {}: sem o cliente OAuth do Google ninguém consegue \
                 entrar (veja \"Login com o Google\" no README).",
                cf::FILE
            );
        }
    }
    Ok(())
}

fn publish_server(cfg: &cf::Config) -> Result {
    println!("==> Preparando as ferramentas");
    let targets = tools::output(Command::new("rustup").args(["target", "list", "--installed"]))
        .ok_or("Não achei o rustup. Instale o Rust em rustup.rs.")?;
    if !targets
        .lines()
        .any(|t| t.trim() == "wasm32-unknown-unknown")
    {
        tools::run(
            Command::new("rustup").args(["target", "add", "wasm32-unknown-unknown"]),
            "instalar o alvo wasm32-unknown-unknown",
        )?;
    }
    let have = tools::output(Command::new("worker-build").arg("--version")).unwrap_or_default();
    if have.trim() != WORKER_BUILD {
        tools::run(
            tools::cargo().args([
                "install",
                "worker-build",
                "--locked",
                "--version",
                WORKER_BUILD,
            ]),
            "instalar o worker-build",
        )?;
    }

    println!("==> Compilando o servidor para WebAssembly");
    let dir = root().join("crates").join("worker");
    tools::run(
        Command::new("worker-build")
            .current_dir(&dir)
            .args(["--release", "--", "--locked"]),
        "compilar o servidor",
    )?;
    let build = dir.join("build");

    block_on(async {
        let cf = Cf::new(cfg)?;
        // The very first publish also creates the database. Say so: a Worker
        // that was deleted took its Durable Object (and the whole chat) with
        // it, and publishing again would quietly start from an empty one.
        let exists = cf.worker_exists().await?;
        if !exists {
            println!();
            println!(
                "ATENÇÃO: o Worker \"{}\" não existe nesta conta, então vou criá-lo com um banco \
                 VAZIO. Se ele já existiu e foi apagado, as contas e as mensagens foram apagadas \
                 com ele e não há como trazê-las de volta por aqui.",
                cfg.worker
            );
            if tools::ask("Digite \"criar\" para continuar:") != "criar" {
                fail!("Cancelado.");
            }
        }
        let bucket = cf.ensure_bucket().await?;
        if !bucket {
            println!(
                "ATENÇÃO: sem o bucket R2 \"{}\" o chat funciona, mas enviar arquivos e a \
                 atualização automática não. Ative o R2 no painel da Cloudflare (R2 Object \
                 Storage; o plano gratuito pede um cartão, mas não cobra nada até 10 GB) e rode \
                 de novo: o bucket é criado sozinho.",
                cfg.bucket
            );
        }

        // Publishing comes first: the secrets endpoint only exists once the
        // Worker does. The address is opened last.
        println!("==> Publicando");
        let res = cf.publish(&build, !exists, bucket).await?;

        println!("==> Guardando os segredos");
        for (name, value) in &cfg.segredos {
            if !value.is_empty() && !OBSOLETE_SECRETS.contains(&name.as_str()) {
                cf.set_secret(name, value).await?;
            }
        }
        // The keys of the SFU the calls once went through. Calls are peer
        // to peer now, so the Worker should not keep them.
        let names = cf.secret_names().await?;
        for name in OBSOLETE_SECRETS
            .iter()
            .filter(|n| names.iter().any(|m| m == *n))
        {
            println!("==> Apagando o segredo {name}, que não é mais usado");
            cf.delete_secret(name).await?;
        }

        println!("==> Abrindo o endereço");
        cf.enable_subdomain().await?;

        let wasm = std::fs::metadata(build.join("index_bg.wasm"))
            .map(|m| m.len())
            .unwrap_or(0);
        let startup = res["startup_time_ms"]
            .as_i64()
            .map(|ms| format!(", abre em {ms} ms"))
            .unwrap_or_default();
        println!("Pronto. {} ({} KB{startup}).", cfg.endereco, wasm / 1024);

        // A brand-new address can take a moment to answer.
        let http = tools::http()?;
        for attempt in 1..=10 {
            let answer = http
                .get(format!("{}/api/version", cfg.endereco))
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            match answer {
                Ok(res) if res.status().is_success() => {
                    let v: serde_json::Value = res.json().await.unwrap_or_default();
                    let got = v["version"].as_str().unwrap_or("?");
                    if got == VERSION {
                        println!("O servidor respondeu: versão {got}.");
                    } else {
                        println!(
                            "ATENÇÃO: o servidor respondeu versão {got}, mas este código é {VERSION}."
                        );
                    }
                    return Ok(());
                }
                other if attempt == 10 => {
                    let why = match other {
                        Ok(res) => format!("HTTP {}", res.status().as_u16()),
                        Err(e) => e.to_string(),
                    };
                    println!(
                        "ATENÇÃO: publiquei, mas {} ainda não respondeu: {why}",
                        cfg.endereco
                    );
                }
                _ => tokio::time::sleep(Duration::from_secs(3)).await,
            }
        }
        Ok(())
    })?
}
