//! `cargo xtask pagina`: the download page, on GitHub Pages. It puts the
//! installer in build/release (the one `cargo xtask release` made) on a
//! GitHub release of its own repository, and the page that links to it at
//! the repository's root, which GitHub Pages serves.
//!
//! The repository holds only the page: no code of the app. It is created
//! the first time, public (GitHub Pages needs that on a free account),
//! under the account the GitHub CLI is signed in to, as "babu-tela", unless
//! build/cloudflare.json names another (`"pagina": "dono/repositorio"`).
//! GitHub is reached with the CLI's own token (`gh auth token`).

use std::io::Cursor;
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

use crate::release::{INSTALLER, STAMP, UPDATE_NAME, release_dir};
use crate::tools::{self, block_on, mb, root};
use crate::{Result, cf, fail, sigv4};

const DEFAULT_REPO: &str = "babu-tela";
const API: &str = "https://api.github.com";
const UPLOADS: &str = "https://uploads.github.com";

/// The page, with `{{...}}` where each release's details go.
const TEMPLATE: &str = include_str!("../pagina/index.html");

pub fn run(args: &[String]) -> Result {
    if let Some(extra) = args.first() {
        fail!("opção desconhecida: {extra}");
    }
    let cfg = cf::load()?;
    let built = installer(&cfg)?;
    let token = gh_token()?;
    block_on(publish(&cfg, &token, &built))?
}

/// An installer to put on the page.
pub struct Built {
    pub version: String,
    pub bytes: Vec<u8>,
    pub sha: String,
    /// When `release` made it (RFC 3339).
    pub made: String,
}

/// The installer in build/release, as `release` recorded it, if it talks
/// to the server in build/cloudflare.json.
pub fn installer(cfg: &cf::Config) -> Result<Built> {
    let dir = release_dir();
    let stamp = dir.join(STAMP);
    let file = dir.join(INSTALLER);
    if !file.exists() || !stamp.exists() {
        fail!("Não achei {INSTALLER} e {STAMP} em build/release. Rode antes: cargo xtask release");
    }
    let made: Value = serde_json::from_str(&std::fs::read_to_string(&stamp)?)?;
    if made["endereco"] != cfg.endereco.as_str() {
        fail!(
            "O instalador aponta para {}, não para {}. Rode de novo: cargo xtask release",
            made["endereco"],
            cfg.endereco
        );
    }
    let bytes = std::fs::read(&file)?;
    Ok(Built {
        version: made["version"].as_str().unwrap_or_default().to_owned(),
        sha: sigv4::sha256_hex(&bytes),
        made: made["gerado"].as_str().unwrap_or_default().to_owned(),
        bytes,
    })
}

/// The GitHub CLI's token, which says it is signed in on this PC.
pub fn gh_token() -> Result<String> {
    let token = tools::output(Command::new("gh").args(["auth", "token"]))
        .map(|t| t.trim().to_owned())
        .unwrap_or_default();
    if token.is_empty() {
        fail!(
            "Não achei o GitHub CLI conectado, que publica a página de download. Instale o gh \
             (cli.github.com) e rode: gh auth login"
        );
    }
    Ok(token)
}

/// Puts `built` on the page: its release (with the installer), then the
/// page that links to it.
pub async fn publish(cfg: &cf::Config, token: &str, built: &Built) -> Result {
    let gh = Gh::new(token)?;
    let me = gh.ok(gh.get("/user").await?, "ler a conta do GitHub")?;
    let login = me["login"].as_str().unwrap_or_default().to_owned();
    let repo = if cfg.pagina.trim().is_empty() {
        format!("{login}/{DEFAULT_REPO}")
    } else {
        cfg.pagina.trim().to_owned()
    };
    let Some((owner, name)) = repo.split_once('/') else {
        fail!(
            "\"pagina\" em {} deveria ser \"dono/repositorio\": {repo}",
            cf::FILE
        );
    };
    let site = format!("https://{}.github.io/{name}/", owner.to_lowercase());
    println!("==> Página de download: {site} (repositório {repo})");

    let branch = ensure_repo(&gh, owner, name, &login, &site).await?;
    // A release needs a commit to point at, so the empty repository gets
    // its first file (the icon) before it.
    put_file(&gh, &repo, &branch, "icone.png", &icon()?, "Ícone").await?;
    let download = release(&gh, &repo, &branch, built).await?;
    let page = render(built, &download);
    let message = format!("Babu Tela {}", built.version);
    put_file(&gh, &repo, &branch, "index.html", page.as_bytes(), &message).await?;
    ensure_pages(&gh, &repo, &branch).await?;
    wait_for_page(&site, &built.version).await;
    Ok(())
}

/// The repository, created the first time; its branch.
async fn ensure_repo(gh: &Gh, owner: &str, name: &str, login: &str, site: &str) -> Result<String> {
    let found = gh.get(&format!("/repos/{owner}/{name}")).await?;
    if found.0 == 200 {
        return Ok(found.1["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_owned());
    }
    println!("    criando o repositório {owner}/{name} (público, só com a página)");
    let body = json!({
        "name": name,
        "description": "Babu Tela: baixe o app para Windows",
        "homepage": site,
        "private": false,
        "has_issues": false,
        "has_projects": false,
        "has_wiki": false,
        "auto_init": false,
    });
    let path = if owner.eq_ignore_ascii_case(login) {
        "/user/repos".to_owned()
    } else {
        format!("/orgs/{owner}/repos")
    };
    let made = gh.ok(
        gh.send("POST", &path, Some(body)).await?,
        "criar o repositório",
    )?;
    Ok(made["default_branch"].as_str().unwrap_or("main").to_owned())
}

/// Writes a file at the repository's root, unless it already holds just that.
async fn put_file(
    gh: &Gh,
    repo: &str,
    branch: &str,
    path: &str,
    content: &[u8],
    message: &str,
) -> Result {
    let url = format!("/repos/{repo}/contents/{path}");
    let current = gh.get(&format!("{url}?ref={branch}")).await?;
    let mut body = json!({ "message": message, "content": tools::base64(content) });
    if current.0 == 200 {
        let same = current.1["content"]
            .as_str()
            .and_then(|b| tools::unbase64(b).ok())
            .is_some_and(|old| old == content);
        if same {
            println!("    {path}: sem mudança");
            return Ok(());
        }
        body["sha"] = current.1["sha"].clone();
        body["branch"] = json!(branch);
    }
    gh.ok(
        gh.send("PUT", &url, Some(body)).await?,
        &format!("enviar {path}"),
    )?;
    println!("    {path}: enviado");
    Ok(())
}

/// The release of `built.version`, with the installer on it; the
/// installer's address.
async fn release(gh: &Gh, repo: &str, branch: &str, built: &Built) -> Result<String> {
    let tag = format!("v{}", built.version);
    let notes = format!(
        "Instalador do Babu Tela {} para Windows.\n\nSHA-256: `{}`",
        built.version, built.sha
    );
    let found = gh
        .get(&format!("/repos/{repo}/releases/tags/{tag}"))
        .await?;
    let rel = if found.0 == 200 {
        found.1
    } else {
        let body = json!({
            "tag_name": tag,
            "target_commitish": branch,
            "name": format!("Babu Tela {}", built.version),
            "body": notes,
            "make_latest": "true",
        });
        gh.ok(
            gh.send("POST", &format!("/repos/{repo}/releases"), Some(body))
                .await?,
            "criar a versão no GitHub",
        )?
    };
    let id = rel["id"].as_i64().ok_or("versão do GitHub sem id")?;
    let assets = rel["assets"].as_array().cloned().unwrap_or_default();
    let old = assets.iter().find(|a| a["name"] == UPDATE_NAME);
    let same = old.is_some_and(|a| a["size"].as_u64() == Some(built.bytes.len() as u64))
        && rel["body"].as_str().is_some_and(|b| b.contains(&built.sha));
    if same {
        println!("    {tag}: o instalador já está lá");
    } else {
        if let Some(a) = old {
            let asset = a["id"].as_i64().unwrap_or_default();
            gh.send(
                "DELETE",
                &format!("/repos/{repo}/releases/assets/{asset}"),
                None,
            )
            .await?;
        }
        if rel["body"].as_str() != Some(notes.as_str()) {
            gh.send(
                "PATCH",
                &format!("/repos/{repo}/releases/{id}"),
                Some(json!({ "body": notes })),
            )
            .await?;
        }
        println!(
            "==> Enviando {UPDATE_NAME} ({}) para a versão {tag}",
            mb(built.bytes.len() as u64)
        );
        let res = gh
            .http
            .post(format!(
                "{UPLOADS}/repos/{repo}/releases/{id}/assets?name={UPDATE_NAME}"
            ))
            .bearer_auth(&gh.token)
            .header("accept", "application/vnd.github+json")
            .header(
                "content-type",
                "application/vnd.microsoft.portable-executable",
            )
            .timeout(Duration::from_secs(900))
            .body(built.bytes.clone())
            .send()
            .await?;
        let status = res.status().as_u16();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        gh.ok((status, body), "enviar o instalador")?;
    }
    Ok(format!(
        "https://github.com/{repo}/releases/download/{tag}/{UPDATE_NAME}"
    ))
}

/// GitHub Pages on, serving the branch's root.
async fn ensure_pages(gh: &Gh, repo: &str, branch: &str) -> Result {
    let url = format!("/repos/{repo}/pages");
    if gh.get(&url).await?.0 == 200 {
        return Ok(());
    }
    println!("    ligando o GitHub Pages");
    let body = json!({ "build_type": "legacy", "source": { "branch": branch, "path": "/" } });
    gh.ok(
        gh.send("POST", &url, Some(body)).await?,
        "ligar o GitHub Pages",
    )?;
    Ok(())
}

/// Waits a while for GitHub Pages to serve the new page (it builds it in
/// about a minute), and says where it is either way.
async fn wait_for_page(site: &str, version: &str) {
    let Ok(http) = tools::http() else { return };
    let wanted = format!("Versão {version}");
    for _ in 0..30 {
        let page = http
            .get(site)
            .header("cache-control", "no-cache")
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        if let Ok(res) = page
            && res.status().is_success()
            && res.text().await.is_ok_and(|t| t.contains(&wanted))
        {
            println!("A página está no ar com a versão {version}: {site}");
            return;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    println!(
        "A página foi enviada, mas o GitHub Pages ainda não a mostra (pode levar alguns \
         minutos): {site}"
    );
}

/// The page for `built`, downloading from `download`.
fn render(built: &Built, download: &str) -> String {
    let date = chrono::DateTime::parse_from_rfc3339(&built.made)
        .map(|d| d.format("%d/%m/%Y").to_string())
        .unwrap_or_default();
    let size = mb(built.bytes.len() as u64).replace('.', ",");
    TEMPLATE
        .replace("{{DOWNLOAD}}", download)
        .replace("{{VERSAO}}", &built.version)
        .replace("{{TAMANHO}}", &size)
        .replace("{{DATA}}", &date)
        .replace("{{SHA256}}", &built.sha)
}

/// The app's icon at the size the page shows it (twice, for sharp screens).
fn icon() -> Result<Vec<u8>> {
    let path = root().join("build").join("appicon.png");
    let img = image::load_from_memory(&std::fs::read(&path)?)?;
    let small = img.resize(176, 176, image::imageops::FilterType::Lanczos3);
    let mut out = Cursor::new(Vec::new());
    small.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// GitHub's REST API.
struct Gh {
    http: reqwest::Client,
    token: String,
}

impl Gh {
    fn new(token: &str) -> Result<Gh> {
        Ok(Gh {
            http: tools::http()?,
            token: token.to_owned(),
        })
    }

    async fn get(&self, path: &str) -> Result<(u16, Value)> {
        self.send("GET", path, None).await
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value)> {
        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut req = self
            .http
            .request(method, format!("{API}{path}"))
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .timeout(Duration::from_secs(60));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await?;
        let status = res.status().as_u16();
        Ok((status, res.json().await.unwrap_or(Value::Null)))
    }

    /// The body of a success, or what GitHub said went wrong.
    fn ok(&self, (status, body): (u16, Value), what: &str) -> Result<Value> {
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let why = body["message"].as_str().unwrap_or("sem detalhes");
        fail!("{what}: o GitHub respondeu {status} ({why})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_gets_every_detail() {
        let built = Built {
            version: "1.2.3".into(),
            bytes: vec![0; 3 * 1024 * 1024 / 2],
            sha: "ab".repeat(32),
            made: "2026-10-20T08:05:16.845093100-03:00".into(),
        };
        let page = render(
            &built,
            "https://github.com/x/y/releases/download/v1.2.3/a.exe",
        );
        assert!(!page.contains("{{"), "{page}");
        assert!(page.contains("href=\"https://github.com/x/y/releases/download/v1.2.3/a.exe\""));
        assert!(page.contains("Versão 1.2.3 · 1,5 MB · 20/10/2026"));
        assert!(page.contains(&"ab".repeat(32)));
    }
}
