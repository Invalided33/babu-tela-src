//! build/cloudflare.json and the Cloudflare API.
//!
//! The file is never committed. It holds:
//!
//! - `contaId`: the account the Worker lives in
//! - `apiToken`: an API token that can edit Workers and R2
//! - `worker`: the Worker's name ("babutela")
//! - `endereco`: the address the app talks to
//! - `bucket`: the R2 bucket for uploads and installers
//! - `r2AccessKeyId`, `r2SecretAccessKey` (optional): R2 keys, only to
//!   publish updates; empty, the API token's own (sigv4.rs)
//! - `armazenamentoGb` (optional): how many GB the uploaded files may add
//!   up to; 10 (R2's free tier) when absent, 0 for no limit
//! - `segredos`: the Worker's secrets (PEPPER, GOOGLE_CLIENT_ID,
//!   GOOGLE_CLIENT_SECRET, TEST_HUB, TURN_KEY_ID, TURN_KEY_TOKEN, ...), sent
//!   on every `cargo xtask servidor`
//! - `pagina` (optional): the download page's repository on GitHub,
//!   "dono/repositorio" (site.rs)

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use reqwest::Method;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::root;
use crate::{Result, fail};

pub const FILE: &str = "build/cloudflare.json";

/// The date the Worker runtime behaves as of.
const COMPATIBILITY_DATE: &str = "2026-10-01";

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub conta_id: String,
    pub api_token: String,
    pub worker: String,
    pub endereco: String,
    pub bucket: String,
    pub r2_access_key_id: String,
    pub r2_secret_access_key: String,
    /// The storage budget for uploads, in GB; None leaves the server's own
    /// default (R2's free 10 GB).
    pub armazenamento_gb: Option<f64>,
    pub segredos: BTreeMap<String, String>,
    /// The download page's repository ("dono/repositorio"); empty for
    /// "babu-tela" under the account the GitHub CLI is signed in to.
    pub pagina: String,
}

impl Config {
    pub fn secret(&self, name: &str) -> Option<&str> {
        self.segredos
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

pub fn load() -> Result<Config> {
    let path = root().join(FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        fail!(
            "Crie {} (o README explica cada campo, em \"Na Cloudflare\").",
            path.display()
        );
    };
    let mut cfg: Config = serde_json::from_str(&text).map_err(|e| format!("{FILE}: {e}"))?;
    for (key, value) in [
        ("contaId", &cfg.conta_id),
        ("apiToken", &cfg.api_token),
        ("worker", &cfg.worker),
        ("endereco", &cfg.endereco),
    ] {
        if value.trim().is_empty() {
            fail!("Falta \"{key}\" em {FILE}.");
        }
    }
    // One place decides what the address looks like, so everything that
    // compares against it agrees.
    cfg.endereco = cfg.endereco.trim().trim_end_matches('/').to_owned();
    Ok(cfg)
}

/// A call to the Cloudflare API that did not succeed, with the codes
/// Cloudflare sent (they say what went wrong better than the status).
#[derive(Debug)]
pub struct CfError {
    pub status: u16,
    pub codes: Vec<i64>,
    pub message: String,
}

impl fmt::Display for CfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CfError {}

impl CfError {
    /// "That does not exist" (and not "no permission" or "try later"):
    /// a 404, or 10007 for a Worker that is not there.
    pub fn missing(&self) -> bool {
        self.status == 404 || self.codes.contains(&10007)
    }
}

pub struct Cf<'a> {
    pub cfg: &'a Config,
    http: reqwest::Client,
}

impl<'a> Cf<'a> {
    pub fn new(cfg: &'a Config) -> Result<Self> {
        Ok(Cf {
            cfg,
            http: crate::tools::http()?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/{path}",
            self.cfg.conta_id
        )
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        what: &str,
    ) -> std::result::Result<Value, CfError> {
        let mut req = self
            .http
            .request(method, self.url(path))
            .bearer_auth(&self.cfg.api_token)
            .timeout(std::time::Duration::from_secs(60));
        if let Some(body) = body {
            req = req.json(&body);
        }
        match req.send().await {
            Ok(res) => read(res, what).await,
            Err(e) => Err(CfError {
                status: 0,
                codes: Vec::new(),
                message: format!("{what} falhou: {e}"),
            }),
        }
    }

    /// The Durable Object only has to be declared the first time, and the
    /// first time is also when the database is born. "I could not check"
    /// must never pass for "it is not there".
    pub async fn worker_exists(&self) -> Result<bool> {
        let path = format!("workers/scripts/{}/settings", self.cfg.worker);
        match self
            .call(Method::GET, &path, None, "ler as configurações do Worker")
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.missing() => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Uploads need a bucket. Until R2 is turned on in the dashboard (the
    /// one step the API cannot take) there is none, and the server goes out
    /// without it; once R2 is on, the bucket is made here.
    pub async fn ensure_bucket(&self) -> Result<bool> {
        if self.cfg.bucket.is_empty() {
            return Ok(false);
        }
        let path = format!("r2/buckets/{}", self.cfg.bucket);
        match self
            .call(Method::GET, &path, None, "procurar o bucket")
            .await
        {
            Ok(_) => Ok(true),
            // 10042: R2 is not enabled on this account yet.
            Err(e) if e.codes.contains(&10042) => Ok(false),
            Err(e) if e.missing() => {
                println!("==> Criando o bucket R2 \"{}\"", self.cfg.bucket);
                self.call(
                    Method::POST,
                    "r2/buckets",
                    Some(json!({ "name": self.cfg.bucket })),
                    "criar o bucket",
                )
                .await?;
                Ok(true)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Uploads the module worker-build made. `first` declares the Durable
    /// Object (a later deploy sends no migration at all, like wrangler).
    pub async fn publish(&self, build: &Path, first: bool, bucket: bool) -> Result<Value> {
        let js = std::fs::read(build.join("index.js"))
            .map_err(|e| format!("{}: {e}", build.join("index.js").display()))?;
        let wasm = std::fs::read(build.join("index_bg.wasm"))
            .map_err(|e| format!("{}: {e}", build.join("index_bg.wasm").display()))?;
        let mut bindings = vec![json!({
            "type": "durable_object_namespace", "name": "HUB", "class_name": "Hub",
        })];
        if bucket {
            bindings.push(json!({
                "type": "r2_bucket", "name": "FILES", "bucket_name": self.cfg.bucket,
            }));
        }
        if let Some(gb) = self.cfg.armazenamento_gb {
            bindings.push(json!({
                "type": "plain_text", "name": "ARMAZENAMENTO_GB", "text": gb.to_string(),
            }));
        }
        let mut metadata = json!({
            "main_module": "index.js",
            "compatibility_date": COMPATIBILITY_DATE,
            // The secrets are set apart from the code, so a deploy must not
            // drop them.
            "keep_bindings": ["secret_text"],
            "bindings": bindings,
            // No request logs kept or sent anywhere (Workers Logs, Logpush):
            // they would hold who connected from which IP, and when. The
            // Worker's own errors are only seen live, in a tail.
            "logpush": false,
            "observability": { "enabled": false },
        });
        if first {
            metadata["migrations"] = json!({ "new_tag": "v1", "new_sqlite_classes": ["Hub"] });
        }
        let form = Form::new()
            .part(
                "metadata",
                Part::text(metadata.to_string()).mime_str("application/json")?,
            )
            .part(
                "index.js",
                Part::bytes(js)
                    .file_name("index.js")
                    .mime_str("application/javascript+module")?,
            )
            .part(
                "index_bg.wasm",
                Part::bytes(wasm)
                    .file_name("index_bg.wasm")
                    .mime_str("application/wasm")?,
            );
        let res = self
            .http
            .put(self.url(&format!("workers/scripts/{}", self.cfg.worker)))
            .bearer_auth(&self.cfg.api_token)
            .timeout(std::time::Duration::from_secs(300))
            .multipart(form)
            .send()
            .await?;
        Ok(read(res, "publicar o Worker").await?)
    }

    pub async fn set_secret(&self, name: &str, value: &str) -> Result {
        let path = format!("workers/scripts/{}/secrets", self.cfg.worker);
        self.call(
            Method::PUT,
            &path,
            Some(json!({ "name": name, "text": value, "type": "secret_text" })),
            &format!("gravar o segredo {name}"),
        )
        .await?;
        Ok(())
    }

    /// The names of the Worker's secrets (never their values: the API does
    /// not give those back).
    pub async fn secret_names(&self) -> Result<Vec<String>> {
        let path = format!("workers/scripts/{}/secrets", self.cfg.worker);
        let list = self
            .call(Method::GET, &path, None, "listar os segredos do Worker")
            .await?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["name"].as_str().map(str::to_owned))
            .collect())
    }

    pub async fn delete_secret(&self, name: &str) -> Result {
        let path = format!("workers/scripts/{}/secrets/{name}", self.cfg.worker);
        self.call(
            Method::DELETE,
            &path,
            None,
            &format!("apagar o segredo {name}"),
        )
        .await?;
        Ok(())
    }

    /// Opens <worker>.<conta>.workers.dev. Done last, so nobody arrives
    /// before the keys.
    pub async fn enable_subdomain(&self) -> Result {
        let path = format!("workers/scripts/{}/subdomain", self.cfg.worker);
        self.call(
            Method::POST,
            &path,
            Some(json!({ "enabled": true, "previews_enabled": false })),
            "habilitar o endereço workers.dev",
        )
        .await?;
        Ok(())
    }
}

async fn read(res: reqwest::Response, what: &str) -> std::result::Result<Value, CfError> {
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if parsed["success"] == true {
        return Ok(parsed["result"].clone());
    }
    let errors = parsed["errors"].as_array().cloned().unwrap_or_default();
    let codes = errors.iter().filter_map(|e| e["code"].as_i64()).collect();
    let mut detail: Vec<String> = errors
        .iter()
        .map(|e| format!("{} {}", e["code"], e["message"].as_str().unwrap_or("")))
        .collect();
    if detail.is_empty() {
        detail.push(format!("HTTP {status}"));
    }
    Err(CfError {
        status,
        codes,
        message: format!("{what} falhou: {}", detail.join("; ")),
    })
}
