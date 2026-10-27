//! `cargo xtask testar`: the Rust tests, then an end-to-end run against the
//! server on Cloudflare.
//!
//!   cargo xtask testar                    tudo
//!   cargo xtask testar --so-servidor      só a parte de ponta a ponta
//!   cargo xtask testar https://outro.endereco
//!
//! The address and the TEST_HUB secret come from build/cloudflare.json (or
//! BABUTELA_URL and BABUTELA_TEST_HUB). Every request carries that secret,
//! so the run gets its own empty database instead of touching the real chat.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::tools::{self, block_on, escape, root};
use crate::{Result, cf, fail};

pub fn run(args: &[String]) -> Result {
    let mut only_server = false;
    let mut address = std::env::var("BABUTELA_URL").ok();
    for arg in args {
        match arg.as_str() {
            "--so-servidor" => only_server = true,
            a if a.starts_with("https://") || a.starts_with("http://") => {
                address = Some(a.to_owned());
            }
            other => fail!("opção desconhecida: {other}"),
        }
    }
    if !only_server {
        println!("==> Testes do app, do instalador e das ferramentas");
        tools::run(
            tools::cargo()
                .current_dir(root())
                .env("PATH", tools::path_with_cmake()?)
                .args(["test", "--workspace"]),
            "os testes em Rust",
        )?;
        println!("==> Testes do servidor");
        tools::run(
            tools::cargo()
                .current_dir(root().join("crates").join("worker"))
                .arg("test"),
            "os testes do servidor",
        )?;
    }

    let cfg = cf::load().ok();
    let base = address
        .or_else(|| cfg.as_ref().map(|c| c.endereco.clone()))
        .ok_or("informe o endereço do servidor (ou BABUTELA_URL)")?
        .trim_end_matches('/')
        .to_owned();
    let secret = std::env::var("BABUTELA_TEST_HUB")
        .ok()
        .or_else(|| {
            cfg.as_ref()
                .and_then(|c| c.secret("TEST_HUB").map(str::to_owned))
        })
        .ok_or("falta o segredo TEST_HUB (em build/cloudflare.json ou BABUTELA_TEST_HUB)")?;

    println!("==> De ponta a ponta contra {base}");
    let started = Instant::now();
    // The whole run is one future, and in a debug build its frames outgrow
    // the main thread's stack (1 MB on Windows): it gets a thread of its own.
    let outcome = std::thread::Builder::new()
        .name("ponta-a-ponta".into())
        .stack_size(64 << 20)
        .spawn(move || -> std::result::Result<(), String> {
            block_on(async {
                let t = T::new(base, &secret)?;
                t.all().await
            })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
        })?
        .join()
        .map_err(|_| "o teste de ponta a ponta parou no meio")?;
    outcome?;
    println!(
        "\nTodos os testes passaram ({} s).",
        started.elapsed().as_secs()
    );
    Ok(())
}

/// A name for this run's database: letters, digits and dashes, at most 40.
fn run_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut h = RandomState::new().build_hasher();
    h.write_u64(millis);
    format!("x{millis:x}-{:06x}", h.finish() & 0xff_ffff)
}

/// Fails the test with a message, like assert! but returning an error.
macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            fail!($($arg)*);
        }
    };
}

/// Fails unless two JSON-ish values are equal.
macro_rules! same {
    ($got:expr, $want:expr, $label:expr) => {{
        let got = json!($got);
        let want = json!($want);
        if got != want {
            fail!("{}: esperava {want}, veio {got}", $label);
        }
    }};
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Value,
}

impl Reply {
    pub(crate) fn token(&self) -> Result<String> {
        match self.body["token"].as_str() {
            Some(t) if !t.is_empty() => Ok(t.to_owned()),
            _ => fail!("sem token na resposta: {} {}", self.status, self.body),
        }
    }

    pub(crate) fn ok(&self, label: &str) -> Result<&Value> {
        check!(self.status < 300, "{label}: {} {}", self.status, self.body);
        Ok(&self.body)
    }

    pub(crate) fn id(&self, label: &str) -> Result<i64> {
        self.ok(label)?["id"]
            .as_i64()
            .ok_or_else(|| format!("{label}: sem id em {}", self.body).into())
    }
}

/// The test's view of the server: every call goes to this run's database.
pub(crate) struct T {
    base: String,
    /// `x-babu-test`'s value: the secret and this run's database.
    pub(crate) header: String,
    pub(crate) run: String,
    http: reqwest::Client,
}

impl T {
    pub(crate) fn new(base: String, secret: &str) -> Result<T> {
        let run = run_name();
        Ok(T {
            base,
            header: format!("{secret}:{run}"),
            run,
            http: tools::http()?,
        })
    }

    async fn call(&self, token: &str, method: Method, url: &str, body: Value) -> Result<Reply> {
        let mut req = self
            .http
            .request(method, format!("{}/api{url}", self.base))
            .header("x-babu-test", &self.header)
            .timeout(Duration::from_secs(30));
        if !token.is_empty() {
            req = req.bearer_auth(token);
        }
        req = if body.is_null() {
            req.header("content-type", "application/json")
        } else {
            req.json(&body)
        };
        let res = req.send().await?;
        let status = res.status().as_u16();
        let body = res.json::<Value>().await.unwrap_or_else(|_| json!({}));
        Ok(Reply { status, body })
    }

    pub(crate) async fn get(&self, token: &str, url: &str) -> Result<Reply> {
        self.call(token, Method::GET, url, Value::Null).await
    }
    pub(crate) async fn post(&self, token: &str, url: &str, body: Value) -> Result<Reply> {
        self.call(token, Method::POST, url, body).await
    }
    /// A message that may meet the flood limit (10 every 10 s per person):
    /// with no network in the way (a local server) the run sends faster
    /// than that, so a 429 waits for the window to pass.
    async fn send_patiently(&self, token: &str, url: &str, body: Value) -> Result<Reply> {
        for _ in 0..12 {
            let sent = self.post(token, url, body.clone()).await?;
            if sent.status != 429 {
                return Ok(sent);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.post(token, url, body).await
    }
    async fn put(&self, token: &str, url: &str) -> Result<Reply> {
        self.call(token, Method::PUT, url, Value::Null).await
    }
    async fn put_json(&self, token: &str, url: &str, body: Value) -> Result<Reply> {
        self.call(token, Method::PUT, url, body).await
    }
    async fn patch(&self, token: &str, url: &str, body: Value) -> Result<Reply> {
        self.call(token, Method::PATCH, url, body).await
    }
    async fn delete(&self, token: &str, url: &str, body: Value) -> Result<Reply> {
        self.call(token, Method::DELETE, url, body).await
    }

    /// A session for `username` in this run's database, the account made
    /// the first time (with `display` as its name). People sign in with
    /// Google, which a test cannot do; this door only exists on the tests'
    /// throwaway databases.
    pub(crate) async fn session(&self, username: &str, display: &str) -> Result<String> {
        self.post(
            "",
            "/test/session",
            json!({ "username": username, "displayName": display }),
        )
        .await?
        .token()
    }

    async fn gateway(&self, token: &str) -> Result<Gateway> {
        Gateway::open(&self.base, &self.header, token).await
    }

    /// One part of an upload, the way the app sends it: the bytes as the
    /// body and the ticket in a header.
    async fn put_part(&self, ticket: &str, n: usize, data: Vec<u8>) -> Result<Reply> {
        let res = self
            .http
            .put(format!("{}/api/uploads/part/{n}", self.base))
            .header("x-babu-test", &self.header)
            .header("x-upload-ticket", ticket)
            .timeout(Duration::from_secs(300))
            .body(data)
            .send()
            .await?;
        let status = res.status().as_u16();
        let body = res.json::<Value>().await.unwrap_or_else(|_| json!({}));
        Ok(Reply { status, body })
    }

    /// Announces a file and sends its parts; gives back the start's answer
    /// and the parts' etags, for `complete`.
    async fn send_parts(
        &self,
        token: &str,
        name: &str,
        data: &[u8],
    ) -> Result<(Value, Vec<String>)> {
        let start = self
            .post(
                token,
                "/uploads",
                json!({ "name": name, "size": data.len() }),
            )
            .await?;
        let start = start.ok("começar o envio")?.clone();
        let ticket = start["ticket"].as_str().unwrap_or("").to_owned();
        let part = start["partSize"].as_u64().unwrap_or(0) as usize;
        let parts = start["parts"].as_u64().unwrap_or(u64::MAX) as usize;
        check!(
            !ticket.is_empty() && part > 0 && parts == data.len().div_ceil(part),
            "plano do envio: {start}"
        );
        let mut etags = Vec::with_capacity(parts);
        for (i, chunk) in data.chunks(part).enumerate() {
            let sent = self.put_part(&ticket, i + 1, chunk.to_vec()).await?;
            let etag = sent.ok(&format!("parte {}", i + 1))?["etag"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            check!(!etag.is_empty(), "parte {} sem etag: {}", i + 1, sent.body);
            etags.push(etag);
        }
        Ok((start, etags))
    }

    /// Sends a whole file and gives back its attachment.
    async fn upload(&self, token: &str, name: &str, data: &[u8]) -> Result<Value> {
        let (start, etags) = self.send_parts(token, name, data).await?;
        let done = self
            .post(
                token,
                "/uploads/complete",
                json!({ "ticket": start["ticket"], "etags": etags }),
            )
            .await?;
        Ok(done.ok("juntar as partes")?.clone())
    }

    /// Reads a stored file (no token: the addresses are public).
    async fn download(&self, url: &str, range: Option<&str>) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .get(format!("{}{url}", self.base))
            .header("x-babu-test", &self.header)
            .timeout(Duration::from_secs(300));
        if let Some(range) = range {
            req = req.header("range", range);
        }
        Ok(req.send().await?)
    }

    async fn all(&self) -> Result {
        let step = |name: &str| println!("ok  {name}");

        let version = self.get("", "/version").await?.body;
        same!(version["name"], "Babu Tela", "nome do servidor");
        println!(
            "    {} — {} {} (banco de teste {})",
            self.base,
            version["name"].as_str().unwrap_or(""),
            version["version"].as_str().unwrap_or(""),
            self.run
        );

        let ana = self.session("ana", "Ana").await?;
        let beto = self.session("beto", "").await?;
        same!(
            self.post("lixo", "/servers", json!({ "name": "x" }))
                .await?
                .status,
            401,
            "token inválido"
        );
        // The tests' door is shut on the real chat: without the tests'
        // header (or with the wrong secret) the request lands there.
        for header in [None, Some("errado:x1")] {
            let mut req = self
                .http
                .post(format!("{}/api/test/session", self.base))
                .json(&json!({ "username": "intruso" }));
            if let Some(h) = header {
                req = req.header("x-babu-test", h);
            }
            same!(
                req.send().await?.status().as_u16(),
                404,
                "sessão de teste no chat de verdade"
            );
        }
        step("contas de teste, e nenhuma porta de teste no chat de verdade");

        // The apps from before Google sign-in are told to update.
        for path in ["/login", "/register"] {
            let old = self
                .post("", path, json!({ "username": "ana", "password": "x" }))
                .await?;
            check!(
                old.status == 410
                    && old.body["error"]
                        .as_str()
                        .is_some_and(|e| e.contains("Google")),
                "{path} das versões antigas: {} {}",
                old.status,
                old.body
            );
        }
        for (method, path) in [
            (Method::GET, "/sessions"),
            (Method::POST, "/sessions/logout-others"),
            (Method::PATCH, "/me/password"),
            (Method::GET, "/me/recovery-codes"),
            (Method::POST, "/recover"),
        ] {
            same!(
                self.call(&ana, method.clone(), path, json!({}))
                    .await?
                    .status,
                404,
                format!("{method} {path} (das senhas) não existe mais")
            );
        }
        step("login só com o Google: senhas, recuperação e sessões saíram");

        // Google itself cannot be driven from here, but the server's half
        // can: a made-up code is refused by Google, which shows that the
        // client and its secret are the ones Google knows (a wrong secret
        // answers 500, and no configuration 503).
        let config = self.get("", "/auth/google").await?;
        let attempt = |code: &str, redirect: &str| {
            json!({
                "code": code,
                "verifier": "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
                "redirectUri": redirect,
            })
        };
        same!(
            self.post("", "/auth/google", attempt("x", "https://site.example"))
                .await?
                .status,
            400,
            "login com um endereço de volta que não é do app"
        );
        let made_up = self
            .post(
                "",
                "/auth/google",
                attempt("4/inventado", "http://127.0.0.1:53682"),
            )
            .await?;
        if config.status == 503 {
            same!(made_up.status, 503, "login com o Google sem configuração");
            println!("--  login com o Google: não configurado no servidor (pulado)");
        } else {
            check!(
                config.status == 200
                    && config.body["clientId"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty()),
                "cliente do Google: {} {}",
                config.status,
                config.body
            );
            check!(
                matches!(made_up.status, 400 | 401),
                "um código inventado deveria ser recusado pelo Google, veio {} {}",
                made_up.status,
                made_up.body
            );
            step(
                "login com o Google: o servidor fala com o Google, que recusa um código inventado",
            );
        }

        let ice = self.get(&ana, "/ice").await?.body;
        let servers = ice["iceServers"].as_array().cloned().unwrap_or_default();
        let urls = |s: &Value| -> Vec<String> {
            match &s["urls"] {
                Value::String(u) => vec![u.clone()],
                Value::Array(list) => list
                    .iter()
                    .filter_map(|u| u.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            }
        };
        check!(
            servers
                .iter()
                .any(|s| urls(s).iter().any(|u| u.starts_with("stun:"))),
            "há pelo menos um servidor STUN: {ice}"
        );
        let turn = servers
            .iter()
            .find(|s| s["username"].as_str().is_some_and(|u| !u.is_empty()));
        if let Some(turn) = turn {
            check!(
                urls(turn)
                    .iter()
                    .any(|u| u.starts_with("turn:") || u.starts_with("turns:")),
                "o TURN tem endereço turn: {turn}"
            );
            check!(
                turn["credential"].as_str().is_some_and(|c| !c.is_empty()),
                "a credencial do TURN vem preenchida"
            );
            // A TURN of one's own (coturn) uses "expiry:user"; Cloudflare's
            // is opaque.
            let user = turn["username"].as_str().unwrap_or("");
            if let Some((expiry, _)) = user.split_once(':')
                && let Ok(expiry) = expiry.parse::<i64>()
            {
                check!(
                    expiry > chrono::Utc::now().timestamp() + 11 * 3600,
                    "a credencial vale por horas"
                );
            }
        }
        same!(self.get("", "/ice").await?.status, 401, "ice sem sessão");
        step(if turn.is_some() {
            "credenciais TURN temporárias"
        } else {
            "servidores ICE (sem TURN configurado)"
        });

        let a = self.gateway(&ana).await?;
        let b = self.gateway(&beto).await?;
        let ready_a = a.next("READY", |_| true).await?;
        let ready_b = b.next("READY", |_| true).await?;
        same!(ready_a["user"]["username"], "ana", "READY da ana");
        // Only its owner hears about the account; a test account has no
        // Google account behind it, and the e-mail never goes to the others.
        same!(ready_a["account"], json!({ "email": "" }), "conta no READY");
        check!(
            ready_a["user"].get("email").is_none() && ready_b["user"].get("email").is_none(),
            "e-mail fora do perfil público: {}",
            ready_a["user"]
        );
        let a_id = ready_a["user"]["id"].as_i64().ok_or("READY sem id")?;
        let b_id = ready_b["user"]["id"].as_i64().ok_or("READY sem id")?;

        let sid = self
            .post(&ana, "/servers", json!({ "name": "Teste" }))
            .await?
            .id("criar servidor")?;
        let srv_a = a.next("SERVER_UPDATE", |d| d["id"] == sid).await?;
        let channels = srv_a["channels"].as_array().cloned().unwrap_or_default();
        same!(channels.len(), 2, "canais padrão");
        let channel = |kind: &str| -> Result<i64> {
            channels
                .iter()
                .find(|c| c["type"] == kind)
                .and_then(|c| c["id"].as_i64())
                .ok_or_else(|| format!("sem canal {kind}").into())
        };
        let text = channel("text")?;
        let voice = channel("voice")?;
        let messages = format!("/channels/{text}/messages");
        step("criação de servidor com canais padrão");

        same!(
            self.get(&beto, &messages).await?.status,
            404,
            "canal de quem não é membro"
        );
        let code = self
            .post(&ana, &format!("/servers/{sid}/invites"), Value::Null)
            .await?
            .ok("convite")?["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.post(
            &beto,
            &format!("/invites/{}/join", escape(&code)),
            Value::Null,
        )
        .await?
        .ok("entrar")?;
        let srv_b = b.next("SERVER_UPDATE", |d| d["id"] == sid).await?;
        same!(
            srv_b["members"].as_array().map_or(0, Vec::len),
            2,
            "membros depois do convite"
        );
        step("convite e entrada no servidor");

        // Messages travel as envelopes (crates/app/src/e2e/msg.rs): the
        // server sees only their shape, so these are made of random bytes,
        // and must come back exactly as they went.
        let env = envelope(&self.run);
        self.post(&beto, &messages, json!({ "e2e": env }))
            .await?
            .ok("mensagem")?;
        let m = a.next("MESSAGE_CREATE", |_| true).await?;
        same!(m["e2e"], env, "o envelope volta igual");
        same!(m["content"], "", "nada em texto claro");
        let mid = m["id"].as_i64().ok_or("mensagem sem id")?;
        same!(
            self.post(&beto, &messages, json!({ "content": "em texto claro" }))
                .await?
                .status,
            400,
            "mensagem sem envelope (app antigo)"
        );
        // A reaction: its mark in the path, the sealed emoji in the body.
        let mark = "0123456789abcdefABCDEF";
        self.put_json(
            &ana,
            &format!("/messages/{mid}/reactions/{mark}"),
            json!({ "d": "cifrada" }),
        )
        .await?
        .ok("reação")?;
        same!(
            b.next("MESSAGE_UPDATE", |_| true).await?["reactions"],
            json!([{ "emoji": mark, "users": [a_id], "data": [[a_id, "cifrada"]] }]),
            "reações"
        );
        same!(
            self.put(&ana, &format!("/messages/{mid}/reactions/{}", escape("👍")))
                .await?
                .status,
            400,
            "emoji em texto claro numa mensagem cifrada"
        );
        let env2 = envelope(&format!("{}-ed", self.run));
        same!(
            self.patch(&ana, &format!("/messages/{mid}"), json!({ "e2e": env2 }))
                .await?
                .status,
            403,
            "editar mensagem alheia"
        );
        self.patch(&beto, &format!("/messages/{mid}"), json!({ "e2e": env2 }))
            .await?
            .ok("editar")?;
        let edited = a.next("MESSAGE_UPDATE", |d| d["e2e"] == env2).await?;
        check!(truthy(&edited["edited"]), "marcada como editada: {edited}");
        b.op(json!({ "op": "typing", "channelId": text }));
        same!(
            a.next("TYPING", |_| true).await?["userId"],
            b_id,
            "digitando"
        );
        // Sent through the gateway: the copy that comes back carries the
        // sender's mark, and sending it again over HTTP (what the app does
        // when the connection falls first) makes no second message.
        let nonce = format!("e2e-{}", self.run);
        let gw_env = envelope(&format!("{}-gw", self.run));
        a.op(json!({
            "op": "message", "channelId": text, "e2e": gw_env, "nonce": nonce,
        }));
        let via_gw = b.next("MESSAGE_CREATE", |d| d["e2e"] == gw_env).await?;
        same!(
            via_gw["nonce"],
            nonce.as_str(),
            "marca na mensagem pelo gateway"
        );
        same!(
            a.next("MESSAGE_CREATE", |d| d["nonce"] == nonce.as_str())
                .await?["id"],
            via_gw["id"],
            "a cópia de quem mandou"
        );
        b.drain("MESSAGE_CREATE");
        let again = self
            .post(&ana, &messages, json!({ "e2e": gw_env, "nonce": nonce }))
            .await?
            .ok("mandar de novo")?["id"]
            .clone();
        same!(again, via_gw["id"], "a mesma mensagem, sem repetir");
        let marker = envelope(&format!("{}-marcador", self.run));
        self.post(&ana, &messages, json!({ "e2e": marker }))
            .await?
            .ok("marcador")?;
        same!(
            b.next("MESSAGE_CREATE", |_| true).await?["e2e"],
            marker,
            "nada repetido antes do marcador"
        );
        a.op(json!({ "op": "message", "channelId": text, "content": "oi", "nonce": "vazia" }));
        same!(
            a.next("SEND_FAILED", |_| true).await?["nonce"],
            "vazia",
            "recusa de texto claro pelo gateway"
        );
        let bad = json!({ "v": 1, "s": "curto", "n": "x", "c": "y" });
        same!(
            self.post(&ana, &messages, json!({ "e2e": bad }))
                .await?
                .status,
            400,
            "envelope malformado"
        );
        step("mensagens cifradas, reações, edição e digitando");

        // The keys (worker/src/e2e.rs): each account publishes its own, the
        // others read them; a session's sealed keys go only to the readers,
        // each only to the one it was sealed for.
        let bundle = |uid: i64| {
            json!({ "v": 1, "uid": uid, "id": "A".repeat(43), "kem": "B".repeat(1622),
                    "sig": "C".repeat(86), "at": 1 })
        };
        a.drain("KEYS_CHANGED");
        self.put_json(&beto, "/e2e/keys", json!({ "keys": bundle(b_id) }))
            .await?
            .ok("publicar chaves")?;
        same!(
            a.next("KEYS_CHANGED", |_| true).await?["userId"],
            b_id,
            "aviso de chaves novas"
        );
        same!(
            self.put_json(&beto, "/e2e/keys", json!({ "keys": bundle(a_id) }))
                .await?
                .status,
            400,
            "chaves em nome de outra pessoa"
        );
        same!(
            self.get(&ana, &format!("/e2e/keys?ids={b_id}"))
                .await?
                .ok("ler chaves")?["keys"][b_id.to_string()],
            bundle(b_id),
            "chaves de quem divide o servidor"
        );
        let reply = self
            .get(&ana, &format!("/channels/{text}/e2e/readers"))
            .await?;
        let readers = reply.ok("leitores")?;
        let mut ids: Vec<i64> = readers["readers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["id"].as_i64())
            .collect();
        ids.sort_unstable();
        same!(
            ids,
            vec![a_id.min(b_id), a_id.max(b_id)],
            "leitores do canal"
        );
        // An encryption session's id; `sid` is still the server's.
        let session_id = "S".repeat(22);
        self.post(
            &ana,
            &format!("/channels/{text}/e2e/sessions"),
            json!({ "sid": session_id, "keys": { a_id.to_string(): "AAAA", b_id.to_string(): "BBBB", "999999": "ZZZZ" } }),
        )
        .await?
        .ok("sessão")?;
        same!(
            self.get(&beto, &format!("/e2e/sessions?ids={session_id}"))
                .await?
                .ok("sessões do beto")?["sessions"],
            json!([{ "sid": session_id, "cid": text, "sender": a_id, "key": "BBBB" }]),
            "cada um só recebe a chave selada para si"
        );
        same!(
            self.post(
                &beto,
                &format!("/channels/{text}/e2e/sessions"),
                json!({ "sid": session_id, "keys": { b_id.to_string(): "XXXX" } }),
            )
            .await?
            .status,
            409,
            "sessão de outra pessoa"
        );
        // A new PC of Ana's asks her other connections for the keys, and
        // only them: Beto hears nothing.
        let a2 = self.gateway(&ana).await?;
        a2.next("READY", |_| true).await?;
        a2.op(json!({ "op": "provision", "data": { "pk": "chave-efemera" } }));
        let asked = a.next("PROVISION", |_| true).await?;
        same!(asked["data"]["pk"], "chave-efemera", "pedido do PC novo");
        a.op(json!({ "op": "provision", "to": asked["from"], "data": { "keys": "seladas" } }));
        same!(
            a2.next("PROVISION", |_| true).await?["data"]["keys"],
            "seladas",
            "resposta só para o PC novo"
        );
        a.op(json!({ "op": "status", "status": "online" }));
        a.next("STATUS", |_| true).await?;
        check!(
            !b.seen("PROVISION", |_| true),
            "o pedido de chaves da ana chegou ao beto"
        );
        a2.close().await;
        b.drain("PRESENCE");
        b.drain("STATUS");
        step("chaves públicas, leitores, sessões seladas e PC novo");

        // GIPHY stickers: only their ids travel and are kept; the apps ask
        // GIPHY for the media themselves.
        same!(ready_a["stickers"], json!([]), "favoritas no READY");
        check!(
            ready_a["giphy"].is_string(),
            "chave do GIPHY no READY: {}",
            ready_a["giphy"]
        );
        // A sticker, like the answer a message is, travels inside the
        // envelope now: whatever plain field comes with one is not kept.
        let (fig, fig2) = ("xT4uQulxzV39haRFjG", "3og0IPxMM0erATueVW");
        let fig_env = envelope(&format!("{}-fig", self.run));
        self.post(
            &beto,
            &messages,
            json!({ "e2e": fig_env, "sticker": fig, "replyTo": mid }),
        )
        .await?
        .ok("figurinha")?;
        let got = a.next("MESSAGE_CREATE", |d| d["e2e"] == fig_env).await?;
        same!(got["sticker"], Value::Null, "figurinha fora do envelope");
        same!(got["reply"], Value::Null, "resposta fora do envelope");
        let fid = got["id"].as_i64().ok_or("figurinha sem id")?;
        let history = self.get(&ana, &messages).await?;
        let stored = history.ok("histórico")?["messages"]
            .as_array()
            .and_then(|l| l.iter().find(|m| m["id"] == fid))
            .cloned()
            .unwrap_or_default();
        same!(stored["e2e"], fig_env, "envelope guardado no histórico");
        same!(stored["content"], "", "histórico sem texto claro");
        a.drain("MESSAGE_CREATE");
        b.drain("MESSAGE_CREATE");

        let favorite = |id: &str| format!("/me/stickers/{id}");
        same!(
            self.put(&ana, &favorite(fig)).await?.ok("favoritar")?["stickers"],
            json!([fig]),
            "favoritas"
        );
        same!(
            a.next("STICKERS", |_| true).await?["stickers"],
            json!([fig]),
            "favoritas nas conexões da pessoa"
        );
        same!(
            self.put(&ana, &favorite(fig))
                .await?
                .ok("favoritar de novo")?["stickers"],
            json!([fig]),
            "favoritar duas vezes"
        );
        same!(
            self.put(&ana, &favorite(fig2))
                .await?
                .ok("outra favorita")?["stickers"],
            json!([fig2, fig]),
            "a mais nova primeiro"
        );
        same!(
            self.put(&ana, &favorite("a-b")).await?.status,
            400,
            "favorita inválida"
        );
        same!(
            self.delete(&ana, &favorite(fig), Value::Null)
                .await?
                .ok("desfavoritar")?["stickers"],
            json!([fig2]),
            "desfavoritar"
        );
        check!(
            !b.seen("STICKERS", |_| true),
            "as favoritas da ana chegaram ao beto"
        );
        a.drain("STICKERS");
        let again = self.gateway(&ana).await?;
        same!(
            again.next("READY", |_| true).await?["stickers"],
            json!([fig2]),
            "favoritas ao conectar de novo"
        );
        again.close().await;
        step("figurinhas: no envelope, e as favoritas");

        // Files go up in parts, straight into R2 (crates/worker/src/files.rs).
        let probe = self
            .post(&beto, "/uploads", json!({ "name": "nota.txt", "size": 9 }))
            .await?;
        if probe.status == 503 {
            println!("--  upload de arquivos: o bucket R2 ainda não está ativado (pulado)");
        } else {
            // A cancelled send is gone, ticket and all.
            let ticket = probe.ok("começar o envio")?["ticket"].clone();
            self.post(&beto, "/uploads/abort", json!({ "ticket": ticket }))
                .await?
                .ok("cancelar o envio")?;
            same!(
                self.post(
                    &beto,
                    "/uploads/complete",
                    json!({ "ticket": ticket, "etags": [] })
                )
                .await?
                .status,
                404,
                "envio cancelado não se completa"
            );

            let up = self
                .upload(&beto, "nota.txt", "conteúdo".as_bytes())
                .await?;
            let url = up["url"].as_str().unwrap_or("").to_owned();
            let name = url
                .strip_prefix("/uploads/")
                .and_then(|n| n.strip_suffix(".txt"))
                .unwrap_or("");
            check!(
                name.len() == 32
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "endereço do anexo: {up}"
            );
            self.post(
                &beto,
                &messages,
                json!({ "e2e": envelope(&format!("{}-nota", self.run)), "files": [{ "url": url }] }),
            )
            .await?
            .ok("anexo")?;
            let got = a
                .next("MESSAGE_CREATE", |d| {
                    d["attachments"].as_array().is_some_and(|l| l.len() == 1)
                })
                .await?;
            // The message only says which upload it holds: the name, size
            // and kind are in its envelope (and the app sends the file
            // itself encrypted, under a neutral name).
            same!(
                got["attachments"],
                json!([{ "url": url }]),
                "anexo só com o endereço"
            );
            let dl = self.download(&url, None).await?;
            same!(
                dl.headers()
                    .get("content-disposition")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                "attachment",
                "download como anexo"
            );
            same!(dl.text().await?, "conteúdo", "conteúdo baixado");
            step("upload de arquivo e download como anexo");

            // 17 MiB go in three parts (8 + 8 + 1), which R2 joins.
            let big: Vec<u8> = (0..17u32 << 20)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
                .collect();
            let (start, mut etags) = self.send_parts(&beto, "video.mp4", &big).await?;
            same!(etags.len(), 3, "partes de um arquivo de 17 MiB");
            let ticket = start["ticket"].clone();
            let wrong = self
                .put_part(ticket.as_str().unwrap_or(""), 1, vec![0; 10])
                .await?;
            same!(wrong.status, 400, "parte do tamanho errado");
            let forged = self.put_part("abc.def", 1, vec![0; 10]).await?;
            same!(forged.status, 403, "parte sem um ticket válido");
            let last = etags.pop();
            same!(
                self.post(
                    &beto,
                    "/uploads/complete",
                    json!({ "ticket": ticket, "etags": etags })
                )
                .await?
                .status,
                400,
                "faltando uma parte"
            );
            etags.extend(last);
            let joined = self
                .post(
                    &beto,
                    "/uploads/complete",
                    json!({ "ticket": ticket, "etags": etags }),
                )
                .await?;
            let video = joined.ok("juntar as partes")?.clone();
            same!(video["kind"], "video", "tipo pela extensão");
            same!(video["size"], big.len(), "tamanho do arquivo grande");
            let url = video["url"].as_str().unwrap_or("").to_owned();
            // A player seeking: a slice across the first two parts.
            let at: usize = (8 << 20) - 8;
            let slice = self
                .download(&url, Some(&format!("bytes={at}-{}", at + 15)))
                .await?;
            same!(slice.status().as_u16(), 206, "pedaço do vídeo");
            same!(
                slice
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                format!("bytes {at}-{}/{}", at + 15, big.len()),
                "Content-Range"
            );
            check!(
                slice.bytes().await?.as_ref() == &big[at..at + 16],
                "o pedaço do vídeo veio diferente"
            );
            let whole = self.download(&url, None).await?;
            same!(
                whole
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                "video/mp4",
                "tipo do vídeo"
            );
            check!(
                whole.bytes().await? == big,
                "o arquivo grande voltou diferente"
            );
            // Never put on a message: dropping it takes it out of R2.
            self.post(&beto, "/uploads/abort", json!({ "ticket": ticket }))
                .await?
                .ok("apagar o arquivo grande")?;
            same!(
                self.download(&url, None).await?.status().as_u16(),
                404,
                "arquivo apagado"
            );
            same!(
                self.post(
                    &beto,
                    "/uploads",
                    json!({ "name": "x", "size": 1u64 << 50 })
                )
                .await?
                .status,
                413,
                "maior do que o R2 guarda"
            );
            // 900 GB pass R2's limit, but not the default storage budget.
            let huge = self
                .post(
                    &beto,
                    "/uploads",
                    json!({ "name": "x.bin", "size": 900_000_000_000u64 }),
                )
                .await?;
            match huge.status {
                507 => {}
                200 => {
                    println!("--  o servidor está sem limite de armazenamento");
                    self.post(
                        &beto,
                        "/uploads/abort",
                        json!({ "ticket": huge.body["ticket"] }),
                    )
                    .await?;
                }
                other => fail!("limite de armazenamento: {other} {}", huge.body),
            }
            step("upload em partes (17 MiB), leitura por pedaços, cancelamento e limites");

            // App 0.9.2 and earlier send the file in one request.
            let form = Form::new().part(
                "file",
                Part::bytes("antigo".as_bytes().to_vec())
                    .file_name("antigo.txt")
                    .mime_str("application/octet-stream")?,
            );
            let legacy = self
                .http
                .post(format!("{}/api/upload", self.base))
                .bearer_auth(&beto)
                .header("x-babu-test", &self.header)
                .multipart(form)
                .send()
                .await?;
            same!(legacy.status().as_u16(), 200, "upload do app antigo");
            let legacy: Value = legacy.json().await?;
            // Such an app sends its messages in plain text: turned down.
            same!(
                self.post(
                    &beto,
                    &messages,
                    json!({ "content": "", "attachments": [legacy] }),
                )
                .await?
                .status,
                400,
                "mensagem do app antigo"
            );
            self.post(
                &beto,
                &messages,
                json!({ "e2e": envelope(&format!("{}-antigo", self.run)), "files": [{ "url": legacy["url"] }] }),
            )
            .await?
            .ok("anexo do upload antigo")?;
            step("upload do app 0.9.2 continua, a mensagem em texto claro não");

            // The server's picture: an image Ana has just uploaded for it.
            // (Only what comes after this counts: Beto's arrival is still
            // queued for Ana.)
            a.drain("SERVER_UPDATE");
            b.drain("SERVER_UPDATE");
            let server_url = format!("/servers/{sid}");
            let icon = self.upload(&ana, "icone.png", b"png de teste").await?;
            let icon_url = icon["url"].as_str().unwrap_or("").to_owned();
            same!(
                self.patch(&beto, &server_url, json!({ "icon": icon_url }))
                    .await?
                    .status,
                403,
                "imagem do servidor sem permissão"
            );
            // Someone else's upload, a file that is no image, one that was
            // never sent, and no address at all will not do.
            let not_hers = self.upload(&beto, "dele.png", b"de outro").await?;
            let text_file = self.upload(&ana, "nota.txt", b"texto").await?;
            for bad in [
                not_hers["url"].clone(),
                text_file["url"].clone(),
                json!("/uploads/0123456789abcdef0123456789abcdef.png"),
                json!(5),
            ] {
                same!(
                    self.patch(&ana, &server_url, json!({ "icon": bad }))
                        .await?
                        .status,
                    400,
                    format!("imagem do servidor inválida ({bad})")
                );
            }
            self.patch(&ana, &server_url, json!({ "icon": icon_url }))
                .await?
                .ok("imagem do servidor")?;
            for gw in [&a, &b] {
                gw.next("SERVER_UPDATE", |d| {
                    d["id"] == sid && d["icon"] == icon_url.as_str()
                })
                .await?;
            }
            // The name alone still goes (the apps up to 0.11 send only it),
            // and the picture stays.
            self.patch(&ana, &server_url, json!({ "name": "Teste" }))
                .await?
                .ok("renomear")?;
            same!(
                a.next("SERVER_UPDATE", |d| d["id"] == sid).await?["icon"],
                icon_url,
                "imagem depois de renomear"
            );
            // A new picture takes the old one's place, whose file goes.
            let second = self.upload(&ana, "outra.webp", b"webp de teste").await?;
            let second_url = second["url"].as_str().unwrap_or("").to_owned();
            self.patch(&ana, &server_url, json!({ "icon": second_url }))
                .await?
                .ok("trocar a imagem")?;
            a.next("SERVER_UPDATE", |d| d["icon"] == second_url.as_str())
                .await?;
            same!(
                self.download(&icon_url, None).await?.status().as_u16(),
                404,
                "imagem antiga apagada"
            );
            self.patch(&ana, &server_url, json!({ "icon": null }))
                .await?
                .ok("tirar a imagem")?;
            a.next("SERVER_UPDATE", |d| d["id"] == sid && d["icon"].is_null())
                .await?;
            same!(
                self.download(&second_url, None).await?.status().as_u16(),
                404,
                "imagem tirada apagada"
            );
            same!(
                self.patch(&ana, &server_url, json!({})).await?.status,
                400,
                "nada para mudar"
            );
            a.drain("SERVER_UPDATE");
            b.drain("SERVER_UPDATE");
            step("imagem do servidor: só de quem gerencia, só uma imagem nova, a antiga apagada");
        }
        same!(
            self.post(
                &beto,
                &messages,
                json!({ "e2e": envelope("fora"), "files": [{ "url": "/uploads/../prosa.db" }] })
            )
            .await?
            .status,
            400,
            "anexo fora da pasta"
        );
        same!(
            self.post(
                &beto,
                &messages,
                json!({
                    "e2e": envelope("inexistente"),
                    "files": [{ "url": format!("/uploads/{}.bin", "a".repeat(32)) }],
                })
            )
            .await?
            .status,
            400,
            "anexo que não existe é recusado"
        );
        let many: Vec<Value> = (0..101)
            .map(|_| json!({ "url": format!("/uploads/{}.bin", "b".repeat(32)) }))
            .collect();
        same!(
            self.post(
                &beto,
                &messages,
                json!({ "e2e": envelope("muitos"), "files": many })
            )
            .await?
            .status,
            400,
            "mais de 100 anexos numa mensagem"
        );
        step("validação de anexos");

        // Files sent directly (worker/src/direct.rs): the message only says
        // the file's id (its name, size and hashes are in the envelope),
        // nothing goes to R2, and the signalling between the two apps only
        // goes between its author and someone who may read it.
        let file_id = "0123456789abcdef0123456789abcdef";
        let dmid = self
            .post(
                &ana,
                &messages,
                json!({
                    "e2e": envelope(&format!("{}-direto", self.run)),
                    "files": [{ "direct": file_id }],
                }),
            )
            .await?
            .id("mensagem com arquivo direto")?;
        same!(
            b.next("MESSAGE_CREATE", |d| d["id"] == dmid).await?["attachments"],
            json!([{ "kind": "direct", "direct": { "id": file_id } }]),
            "arquivo direto na mensagem: só o id"
        );
        a.drain("MESSAGE_CREATE");
        same!(
            self.post(
                &ana,
                &messages,
                json!({ "e2e": envelope("xyz"), "files": [{ "direct": "xyz" }] })
            )
            .await?
            .status,
            400,
            "id de arquivo direto inválido"
        );
        let signal = |to: i64, file: &str, sid: &str, what: &str| {
            json!({ "op": "direct", "to": to, "msg": dmid, "file": file,
                    "data": { "sid": sid, "description": what } })
        };
        b.op(signal(a_id, file_id, "1", "oferta"));
        same!(
            a.next("DIRECT", |_| true).await?,
            json!({ "from": b_id, "msg": dmid, "file": file_id,
                    "data": { "sid": "1", "description": "oferta" } }),
            "pedido de quem pode ler"
        );
        a.op(signal(b_id, file_id, "1", "resposta"));
        same!(
            b.next("DIRECT", |_| true).await?["data"]["description"],
            "resposta",
            "resposta de quem enviou"
        );
        b.op(signal(a_id, &"f".repeat(32), "2", "oferta"));
        same!(
            b.next("DIRECT", |_| true).await?["data"],
            json!({ "sid": "2", "error": "gone" }),
            "arquivo que não está na mensagem"
        );
        // Caio is not on the server: he may not ask, nor be reached.
        let caio = self.session("caio", "Caio").await?;
        let c = self.gateway(&caio).await?;
        let c_id = c.next("READY", |_| true).await?["user"]["id"]
            .as_i64()
            .ok_or("READY sem id")?;
        c.op(signal(a_id, file_id, "3", "oferta"));
        same!(
            c.next("DIRECT", |_| true).await?["data"],
            json!({ "sid": "3", "error": "denied" }),
            "pedido de quem não pode ler"
        );
        a.op(signal(c_id, file_id, "4", "oferta"));
        same!(
            a.next("DIRECT", |_| true).await?["data"],
            json!({ "sid": "4", "error": "denied" }),
            "sinal para quem não pode ler"
        );
        // A refusal is not answered with another one.
        a.op(
            json!({ "op": "direct", "to": c_id, "msg": dmid, "file": file_id,
                     "data": { "sid": "5", "error": "gone" } }),
        );
        // The server takes one connection's messages in order: once Ana has
        // her STATUS back, whatever her steps caused has gone out.
        a.op(json!({ "op": "status", "status": "online" }));
        a.next("STATUS", |d| d["status"] == "online").await?;
        check!(
            !a.seen("DIRECT", |_| true) && !c.seen("DIRECT", |_| true),
            "sinal de arquivo direto que não devia passar"
        );
        // Beto hears of the status too; taken here, so a later step does not.
        b.next("PRESENCE", |d| d["userId"] == a_id).await?;
        c.close().await;
        step(
            "arquivos diretos: só o id no servidor, e a sinalização só entre quem enviou e quem pode ler",
        );

        // The largest envelope goes, whole; one past it does not.
        let sized = |n: usize, tag: &str| json!({ "v": 1, "s": "0123456789abcdefABCDEF", "n": tag, "c": "a".repeat(n) });
        self.post(
            &ana,
            &messages,
            json!({ "e2e": sized(800_000, "0123456789abcdefABCDEG") }),
        )
        .await?
        .ok("envelope do maior tamanho")?;
        same!(
            b.next("MESSAGE_CREATE", |d| d["e2e"]["c"]
                .as_str()
                .is_some_and(|c| c.len() == 800_000))
                .await?["author"]["id"],
            a_id,
            "envelope grande chega inteiro"
        );
        same!(
            self.post(
                &ana,
                &messages,
                json!({ "e2e": sized(800_001, "0123456789abcdefABCDEH") })
            )
            .await?
            .status,
            400,
            "envelope grande demais"
        );
        step("envelopes de até 800 mil caracteres (uma mensagem de 100 mil)");

        same!(
            self.post(
                &beto,
                &format!("/servers/{sid}/channels"),
                json!({ "name": "x" })
            )
            .await?
            .status,
            403,
            "criar canal sem permissão"
        );
        same!(
            self.delete(
                &beto,
                &format!("/servers/{sid}/members/{a_id}"),
                Value::Null
            )
            .await?
            .status,
            403,
            "expulsar a dona"
        );
        let rid = self
            .post(
                &ana,
                &format!("/servers/{sid}/roles"),
                json!({ "name": "Mod", "color": "#f2b84b", "perms": 1 | 2 | 8 | 16 }),
            )
            .await?
            .id("criar cargo")?;
        self.put(&ana, &format!("/servers/{sid}/members/{b_id}/roles/{rid}"))
            .await?
            .ok("dar cargo")?;
        self.post(
            &beto,
            &format!("/servers/{sid}/channels"),
            json!({ "name": "Novo Canal" }),
        )
        .await?
        .ok("criar canal com cargo")?;
        same!(
            self.patch(&beto, &format!("/roles/{rid}"), json!({ "perms": 4095 }))
                .await?
                .status,
            403,
            "aumentar o próprio cargo"
        );
        same!(
            self.post(
                &beto,
                &format!("/servers/{sid}/roles"),
                json!({ "name": "Admin", "perms": 128 })
            )
            .await?
            .status,
            403,
            "criar cargo de administrador"
        );
        step("cargos, permissões e bloqueio de escalonamento");

        let everyone = srv_a["roles"]
            .as_array()
            .and_then(|roles| roles.iter().find(|r| truthy(&r["is_everyone"])))
            .and_then(|r| r["id"].as_i64())
            .ok_or("sem o cargo @everyone")?;
        self.delete(
            &ana,
            &format!("/servers/{sid}/members/{b_id}/roles/{rid}"),
            Value::Null,
        )
        .await?
        .ok("tirar cargo")?;
        let overrides = format!("/channels/{text}/overrides/{everyone}");
        self.put_json(&ana, &overrides, json!({ "allow": 0, "deny": 1 }))
            .await?
            .ok("override")?;
        b.next("SERVER_UPDATE", |d| {
            d["channels"]
                .as_array()
                .is_some_and(|c| !c.iter().any(|c| c["id"] == text))
        })
        .await?;
        same!(
            self.get(&beto, &messages).await?.status,
            404,
            "canal escondido"
        );
        self.put_json(&ana, &overrides, json!({ "allow": 0, "deny": 0 }))
            .await?
            .ok("limpar override")?;
        step("permissões por canal escondem o canal");

        let target = self
            .post(&ana, &messages, json!({ "e2e": envelope("denunciavel") }))
            .await?
            .id("mensagem a denunciar")?;
        let report = format!("/messages/{target}/report");
        same!(
            self.post(&ana, &report, json!({ "reason": "spam" }))
                .await?
                .status,
            400,
            "denunciar a própria mensagem"
        );
        self.post(
            &beto,
            &report,
            json!({ "reason": "spam", "detail": "propaganda" }),
        )
        .await?
        .ok("denunciar")?;
        let with_report = a
            .next("SERVER_UPDATE", |d| {
                d["reports"].as_array().is_some_and(|r| r.len() == 1)
            })
            .await?;
        same!(
            with_report["reports"][0]["reason"],
            "Spam: propaganda",
            "motivo"
        );
        let seen_by_b = b.next("SERVER_UPDATE", |d| d["id"] == sid).await?;
        check!(
            seen_by_b["reports"].as_array().is_none_or(Vec::is_empty),
            "membro comum não vê denúncias: {}",
            seen_by_b["reports"]
        );
        let report_id = with_report["reports"][0]["id"]
            .as_i64()
            .ok_or("denúncia sem id")?;
        self.delete(&ana, &format!("/reports/{report_id}"), Value::Null)
            .await?
            .ok("resolver denúncia")?;
        a.next("SERVER_UPDATE", |d| {
            d["reports"].as_array().is_some_and(Vec::is_empty)
        })
        .await?;
        step("denúncia chega só para a moderação");

        a.op(json!({ "op": "voice_join", "channelId": voice }));
        b.op(json!({ "op": "voice_join", "channelId": voice, "mute": true }));
        let vs = a
            .next("VOICE_STATE", |d| {
                d["users"].as_array().is_some_and(|u| u.len() == 2)
            })
            .await?;
        let b_state = vs["users"]
            .as_array()
            .and_then(|u| u.iter().find(|u| u["userId"] == b_id))
            .cloned()
            .unwrap_or_default();
        same!(b_state["mute"], true, "beto entrou mudo");
        a.op(json!({ "op": "signal", "to": b_id, "data": { "candidate": "teste" } }));
        same!(
            b.next("SIGNAL", |_| true).await?,
            json!({ "from": a_id, "data": { "candidate": "teste" } }),
            "sinalização"
        );
        // Only what comes after Beto's leave counts: an "only Ana" from
        // before he joined may still be queued.
        a.drain("VOICE_STATE");
        b.op(json!({ "op": "voice_leave" }));
        a.next("VOICE_STATE", |d| {
            d["users"].as_array().is_some_and(|u| u.len() == 1)
        })
        .await?;
        step("voz: entrada, estado e sinalização WebRTC");

        // Calls are peer to peer: the server only relays the signalling,
        // and only between two people in the same call. Ana is still in it,
        // Beto has left, so what she sends him goes nowhere.
        a.op(json!({ "op": "signal", "to": b_id, "data": { "candidate": "depois" } }));
        // The server takes one connection's messages in order: once it has
        // answered this one, it has dealt with the signal too.
        a.op(json!({ "op": "status", "status": "online" }));
        a.next("STATUS", |d| d["status"] == "online").await?;
        // Beto hears of it too; taken here, so a later step does not.
        b.next("PRESENCE", |d| {
            d["userId"] == a_id && d["status"] == "online"
        })
        .await?;
        b.op(json!({ "op": "voice_join", "channelId": voice }));
        a.next("VOICE_STATE", |d| {
            d["users"].as_array().is_some_and(|u| u.len() == 2)
        })
        .await?;
        b.op(json!({ "op": "signal", "to": a_id, "data": { "candidate": "de volta" } }));
        same!(
            a.next("SIGNAL", |_| true).await?["data"]["candidate"],
            "de volta",
            "sinalização depois de voltar"
        );
        check!(
            !b.seen("SIGNAL", |d| d["data"]["candidate"] == "depois"),
            "a sinalização para quem saiu da chamada não deveria chegar"
        );
        // A brief WebSocket drop that the server has not noticed yet
        // (common with network blips: Cloudflare's edge may hold a dead
        // connection for a while, so the old sock is still alive on the
        // server when the new one shows up) must not drop the user from
        // the call aos olhos dos outros. A fresh gateway for Beto that
        // joins the same channel stands in for the reconnected app
        // (crates/app/src/core.rs on READY): voice_join on it moves the
        // voice over silently (gateway.rs on the Worker), so Ana only
        // sees Beto with the new connection, never without him.
        a.drain("VOICE_STATE");
        let b2 = self.gateway(&beto).await?;
        b2.next("READY", |_| true).await?;
        b2.op(json!({ "op": "voice_join", "channelId": voice }));
        let vs = a
            .next("VOICE_STATE", |d| {
                d["users"].as_array().is_some_and(|u| u.len() == 2)
            })
            .await?;
        check!(
            vs["users"]
                .as_array()
                .is_some_and(|u| u.iter().any(|x| x["userId"] == b_id)),
            "voz: a VOICE_STATE depois do reconnect devia manter Beto na chamada: {vs}"
        );
        check!(
            !a.seen("VOICE_STATE", |d| {
                d["users"].as_array().is_some_and(|u| u.len() < 2)
            }),
            "voz: Ana recebeu uma VOICE_STATE sem Beto durante o reconnect dele"
        );
        step("voz: um usuário reconectar não o tira da chamada aos olhos dos outros");

        // The old socket keeps running (silently kicked by the server):
        // close it and take the new one as b from here on.
        b.close().await;
        let b = b2;

        b.op(json!({ "op": "voice_leave" }));
        a.next("VOICE_STATE", |d| {
            d["users"].as_array().is_some_and(|u| u.len() == 1)
        })
        .await?;
        // No media server: nothing of the SFU is left on the server.
        let ice = self.get(&ana, "/ice").await?.body;
        check!(ice.get("sfu").is_none(), "o /ice ainda fala do SFU: {ice}");
        same!(
            self.post(&ana, "/sfu/publish", json!({})).await?.status,
            404,
            "a rota do SFU não existe mais"
        );
        step("chamadas só entre as pessoas: sinalização só dentro da chamada, sem SFU");

        b.op(json!({ "op": "status", "status": "dnd" }));
        same!(b.next("STATUS", |_| true).await?["status"], "dnd", "status");
        a.next("PRESENCE", |d| d["userId"] == b_id && d["status"] == "dnd")
            .await?;
        b.op(json!({ "op": "status", "status": "invisible" }));
        a.next("PRESENCE", |d| {
            d["userId"] == b_id && d["status"] == "offline"
        })
        .await?;
        b.op(json!({ "op": "status", "status": "online" }));
        a.next("PRESENCE", |d| {
            d["userId"] == b_id && d["status"] == "online"
        })
        .await?;
        step("status escolhido (inclusive invisível) chega para quem convive");

        self.post(&ana, "/friends", json!({ "username": "beto" }))
            .await?
            .ok("pedido de amizade")?;
        same!(
            b.next("FRIEND_UPDATE", |_| true).await?["state"],
            "incoming",
            "pedido chegou"
        );
        self.put(&beto, &format!("/friends/{a_id}"))
            .await?
            .ok("aceitar")?;
        same!(
            a.next("FRIEND_UPDATE", |d| d["state"] == "accepted")
                .await?["user"]["username"],
            "beto",
            "amizade aceita"
        );
        let dm = self
            .post(&ana, "/dms", json!({ "userId": b_id }))
            .await?
            .id("abrir conversa")?;
        let dm_messages = format!("/channels/{dm}/messages");
        self.post(&ana, &dm_messages, json!({ "e2e": envelope("privado") }))
            .await?
            .ok("dm")?;
        same!(
            b.next("MESSAGE_CREATE", |d| d["channel_id"] == dm).await?["e2e"],
            envelope("privado"),
            "mensagem direta"
        );
        step("amigos e mensagens diretas");

        self.put(&beto, &format!("/blocks/{a_id}"))
            .await?
            .ok("bloquear")?;
        same!(
            b.next("BLOCKS", |_| true).await?["ids"],
            json!([a_id]),
            "bloqueados"
        );
        a.next("FRIEND_REMOVE", |d| d["userId"] == b_id).await?;
        same!(
            self.post(&ana, &dm_messages, json!({ "e2e": envelope("bloqueado") }))
                .await?
                .status,
            403,
            "mensagem para quem bloqueou"
        );
        same!(
            self.post(&ana, "/friends", json!({ "username": "beto" }))
                .await?
                .status,
            403,
            "amizade com quem bloqueou"
        );
        self.delete(&beto, &format!("/blocks/{a_id}"), Value::Null)
            .await?
            .ok("desbloquear")?;
        self.send_patiently(&ana, &dm_messages, json!({ "e2e": envelope("voltou") }))
            .await?
            .ok("dm após desbloqueio")?;
        step("bloqueio impede mensagem direta e amizade");

        self.put(&ana, &format!("/servers/{sid}/bans/{b_id}"))
            .await?
            .ok("banir")?;
        same!(
            b.next("SERVER_REMOVE", |_| true).await?["id"],
            sid,
            "removido do servidor"
        );
        same!(
            self.post(
                &beto,
                &format!("/invites/{}/join", escape(&code)),
                Value::Null
            )
            .await?
            .status,
            403,
            "banido não volta"
        );
        step("banimento impede nova entrada");

        a.close().await;
        same!(
            b.next("PRESENCE", |d| d["userId"] == a_id).await?["status"],
            "offline",
            "presença"
        );
        step("presença offline ao desconectar");
        b.close().await;

        let mut codes = Vec::new();
        for i in 0..12 {
            codes.push(
                self.post(
                    &ana,
                    &dm_messages,
                    json!({ "e2e": envelope(&format!("flood{i}")) }),
                )
                .await?
                .status,
            );
        }
        check!(
            codes.contains(&429) && codes.iter().filter(|c| **c == 200).count() <= 10,
            "limite de mensagens: {codes:?}"
        );
        step("limite de mensagens seguidas");

        // Another app of each (ana's second device, beto's).
        let ana2 = self.session("ana", "").await?;
        let beto2 = self.session("beto", "").await?;
        let c = self.gateway(&ana2).await?;
        c.next("READY", |_| true).await?;
        let dan = self.session("dan", "").await?;
        self.post(&dan, "/friends", json!({ "username": "ana" }))
            .await?
            .ok("pedido do dan")?;
        let dan_id = c.next("FRIEND_UPDATE", |_| true).await?["user"]["id"]
            .as_i64()
            .ok_or("pedido sem id")?;
        self.post(&dan, "/servers", json!({ "name": "Do Dan" }))
            .await?
            .ok("servidor do dan")?;
        // No password to confirm it any more: the app asks first.
        self.delete(&dan, "/me", Value::Null)
            .await?
            .ok("excluir conta")?;
        same!(
            c.next("USER_DELETED", |_| true).await?["userId"],
            dan_id,
            "aviso aos contatos"
        );
        same!(
            self.get(&dan, "/ice").await?.status,
            401,
            "sessão de conta excluída"
        );
        step("excluir conta remove o usuário e avisa os contatos");
        // Beto is told of Ana's presence, which is how this test knows the
        // server has seen her app close: her last connection going tells
        // him (connected before, so he hears all of it).
        let watch = self.gateway(&beto2).await?;
        watch.next("READY", |_| true).await?;
        let about_ana = |d: &Value| d["userId"] == a_id;
        c.close().await;
        watch.next("PRESENCE", about_ana).await?;

        // Whoever picks "invisible" stays invisible after closing the app.
        let d1 = self.gateway(&ana2).await?;
        d1.next("READY", |_| true).await?;
        watch.next("PRESENCE", about_ana).await?;
        d1.op(json!({ "op": "status", "status": "invisible" }));
        same!(
            d1.next("STATUS", |_| true).await?["status"],
            "invisible",
            "status"
        );
        same!(
            watch.next("PRESENCE", about_ana).await?["status"],
            "offline",
            "invisível para os outros"
        );
        // Closed for good (the server saw it: Beto is told) before the app
        // opens again.
        d1.close().await;
        watch.next("PRESENCE", about_ana).await?;
        let d2 = self.gateway(&ana2).await?;
        same!(
            d2.next("READY", |_| true).await?["user"]["status"],
            "invisible",
            "status depois de reabrir"
        );
        d2.close().await;
        watch.close().await;
        step("status escolhido sobrevive a fechar e reabrir o app");

        self.private_servers().await?;
        step("servidor privado: o convite só pede, e quem pode expulsar aceita ou recusa");

        let home = self
            .http
            .get(format!("{}/", self.base))
            .header("x-babu-test", &self.header)
            .send()
            .await?;
        same!(home.status().as_u16(), 200, "endereço raiz");
        check!(
            home.headers().get("access-control-allow-origin").is_none(),
            "sem CORS: nenhuma página de outro site lê as respostas"
        );
        let page = home.text().await?;
        check!(
            page.contains("/updates/Babu-Tela-Setup.exe") && !page.to_lowercase().contains("<html"),
            "a raiz aponta para o instalador, sem versão web: {page}"
        );
        let unknown = self
            .http
            .get(format!("{}/api/nada", self.base))
            .header("x-babu-test", &self.header)
            .send()
            .await?;
        same!(unknown.status().as_u16(), 404, "rota desconhecida");
        same!(
            self.get("", "/../index.html").await?.status,
            404,
            "nenhuma página web"
        );
        step("sem versão web: a raiz aponta para o app, e rota desconhecida");
        Ok(())
    }

    /// Private servers, with people of their own: an invite only lets
    /// someone ask to come in, and whoever may expel members lets them in
    /// or turns them down (a ban does too).
    async fn private_servers(&self) -> Result {
        let eva = self.session("eva", "Eva").await?;
        let fabio = self.session("fabio", "Fábio").await?;
        let gabi = self.session("gabi", "Gabi").await?;
        let hugo = self.session("hugo", "Hugo").await?;
        let e = self.gateway(&eva).await?;
        let f = self.gateway(&fabio).await?;
        let h = self.gateway(&hugo).await?;
        e.next("READY", |_| true).await?;
        let f_ready = f.next("READY", |_| true).await?;
        let h_ready = h.next("READY", |_| true).await?;
        same!(f_ready["requests"], json!([]), "nenhum pedido no READY");
        let f_id = f_ready["user"]["id"].as_i64().ok_or("READY sem id")?;
        let h_id = h_ready["user"]["id"].as_i64().ok_or("READY sem id")?;

        let sid = self
            .post(&eva, "/servers", json!({ "name": "Fechado" }))
            .await?
            .id("criar o servidor")?;
        let made = e.next("SERVER_UPDATE", |d| d["id"] == sid).await?;
        same!(made["private"], false, "servidor novo é aberto");
        let text = made["channels"]
            .as_array()
            .and_then(|list| list.iter().find(|c| c["type"] == "text"))
            .and_then(|c| c["id"].as_i64())
            .ok_or("sem canal de texto")?;
        let messages = format!("/channels/{text}/messages");
        let code = self
            .post(&eva, &format!("/servers/{sid}/invites"), Value::Null)
            .await?
            .ok("convite")?["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let join = format!("/invites/{}/join", escape(&code));
        // Gabi comes in while it is still open; she cannot close it.
        self.post(&gabi, &join, Value::Null)
            .await?
            .ok("entrar no servidor aberto")?;
        let server = format!("/servers/{sid}");
        same!(
            self.patch(&gabi, &server, json!({ "private": true }))
                .await?
                .status,
            403,
            "fechar o servidor sem permissão"
        );
        same!(
            self.patch(&eva, &server, json!({ "private": "sim" }))
                .await?
                .status,
            400,
            "“private” que não é verdadeiro nem falso"
        );
        self.patch(&eva, &server, json!({ "private": true }))
            .await?
            .ok("fechar o servidor")?;
        e.next("SERVER_UPDATE", |d| d["id"] == sid && d["private"] == true)
            .await?;
        // One connection keeps its events in order: what is queued before
        // this is older (Gabi's arrival), and only what comes next counts.
        e.drain("SERVER_UPDATE");

        // Fábio asks, and waits outside.
        let asked = self.post(&fabio, &join, Value::Null).await?;
        let asked = asked.ok("pedir para entrar")?;
        same!(asked["pending"], true, "pedido em vez de entrada");
        same!(asked["id"], sid, "servidor do pedido");
        let his = f.next("JOIN_REQUESTS", |_| true).await?;
        same!(
            his["requests"][0]["server"]["name"],
            "Fechado",
            "o pedido para quem pediu"
        );
        let waiting = |n: usize| {
            move |d: &Value| {
                d["id"] == sid && d["requests"].as_array().is_some_and(|r| r.len() == n)
            }
        };
        let seen = e.next("SERVER_UPDATE", waiting(1)).await?;
        e.drain("SERVER_UPDATE");
        same!(
            seen["requests"][0]["user"]["username"],
            "fabio",
            "quem pediu"
        );
        same!(
            self.get(&fabio, &messages).await?.status,
            404,
            "canal de quem só pediu"
        );
        // A second app of his knows of it from the start.
        let f2 = self.gateway(&fabio).await?;
        same!(
            f2.next("READY", |_| true).await?["requests"][0]["server"]["id"],
            sid,
            "pedido no READY"
        );
        f2.close().await;
        // Asking again is the same request.
        same!(
            self.post(&fabio, &join, Value::Null)
                .await?
                .ok("pedir de novo")?["pending"],
            true,
            "pedido repetido"
        );
        // Gabi may not decide, nor see who asked.
        let request = format!("/servers/{sid}/requests/{f_id}");
        same!(
            self.put(&gabi, &request).await?.status,
            403,
            "aceitar sem poder expulsar"
        );
        same!(
            self.delete(&gabi, &request, Value::Null).await?.status,
            403,
            "recusar sem poder expulsar"
        );
        let g = self.gateway(&gabi).await?;
        let g_ready = g.next("READY", |_| true).await?;
        let her_view = g_ready["servers"]
            .as_array()
            .and_then(|list| list.iter().find(|s| s["id"] == sid))
            .cloned()
            .unwrap_or_default();
        same!(
            her_view["requests"],
            json!([]),
            "pedidos para quem não decide"
        );
        same!(her_view["private"], true, "o servidor é privado para ela");

        // He gives up, then asks again; Eva turns him down.
        let mine =
            |n: usize| move |d: &Value| d["requests"].as_array().is_some_and(|r| r.len() == n);
        self.delete(&fabio, &request, Value::Null)
            .await?
            .ok("desistir do pedido")?;
        f.next("JOIN_REQUESTS", mine(0)).await?;
        e.next("SERVER_UPDATE", waiting(0)).await?;
        e.drain("SERVER_UPDATE");
        self.post(&fabio, &join, Value::Null)
            .await?
            .ok("pedir outra vez")?;
        f.next("JOIN_REQUESTS", mine(1)).await?;
        e.next("SERVER_UPDATE", waiting(1)).await?;
        e.drain("SERVER_UPDATE");
        self.delete(&eva, &request, Value::Null)
            .await?
            .ok("recusar")?;
        same!(
            f.next("JOIN_DENIED", |_| true).await?,
            json!({ "id": sid, "name": "Fechado" }),
            "recusado"
        );
        f.next("JOIN_REQUESTS", mine(0)).await?;
        e.next("SERVER_UPDATE", waiting(0)).await?;
        e.drain("SERVER_UPDATE");
        same!(
            self.put(&eva, &request).await?.status,
            404,
            "aceitar um pedido que não existe"
        );

        // He asks once more, and Eva lets him in.
        self.post(&fabio, &join, Value::Null)
            .await?
            .ok("pedir mais uma vez")?;
        f.next("JOIN_REQUESTS", mine(1)).await?;
        e.next("SERVER_UPDATE", waiting(1)).await?;
        e.drain("SERVER_UPDATE");
        self.put(&eva, &request).await?.ok("aceitar")?;
        let joined = f.next("SERVER_UPDATE", |d| d["id"] == sid).await?;
        same!(
            joined["members"].as_array().map_or(0, Vec::len),
            3,
            "membros depois de aceito"
        );
        f.next("JOIN_REQUESTS", mine(0)).await?;
        self.get(&fabio, &messages)
            .await?
            .ok("ler o canal depois de aceito")?;

        // A ban turns a request down too, and no new one comes.
        self.post(&hugo, &join, Value::Null)
            .await?
            .ok("pedido do hugo")?;
        e.next("SERVER_UPDATE", waiting(1)).await?;
        e.drain("SERVER_UPDATE");
        self.put(&eva, &format!("/servers/{sid}/bans/{h_id}"))
            .await?
            .ok("banir quem pediu")?;
        same!(
            h.next("JOIN_DENIED", |_| true).await?["id"],
            sid,
            "banido é recusado"
        );
        e.next("SERVER_UPDATE", waiting(0)).await?;
        same!(
            self.post(&hugo, &join, Value::Null).await?.status,
            403,
            "banido não pede de novo"
        );

        // Open again: the invite lets people straight in.
        self.patch(&eva, &server, json!({ "private": false }))
            .await?
            .ok("abrir o servidor")?;
        let ivo = self.session("ivo", "Ivo").await?;
        let came = self.post(&ivo, &join, Value::Null).await?;
        check!(
            came.ok("entrar depois de aberto")?.get("pending").is_none(),
            "entrada direta depois de aberto: {}",
            came.body
        );
        self.get(&ivo, &messages)
            .await?
            .ok("ler o canal depois de entrar")?;
        for gw in [e, f, g, h] {
            gw.close().await;
        }
        Ok(())
    }
}

/// An envelope of the shape the apps send (crates/app/src/e2e/msg.rs),
/// made from `tag` (the same tag, the same envelope): the server sees only
/// its shape, and must hand it back exactly as it came.
pub(crate) fn envelope(tag: &str) -> Value {
    use sha2::{Digest, Sha256};
    let h = hex::encode(Sha256::digest(tag.as_bytes()));
    json!({ "v": 1, "s": &h[..22], "n": &h[22..44], "c": h })
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

// ---------------------------------------------------------------- for `cargo xtask medir`

/// People with accounts in a test database, all in one server of the
/// first one's, and the server's text channel.
pub(crate) struct Room {
    pub(crate) tokens: Vec<String>,
    pub(crate) text: i64,
}

/// What the measured chat says (with some formatting, a link and an emoji,
/// as a real one would).
const CHATTER: [&str; 8] = [
    "Bom dia, pessoal! Alguém vai jogar hoje à noite?",
    "Eu vou! Mas só depois das **20h**, tenho reunião antes.",
    "Olha esse link: https://example.com/noticia-interessante",
    "Fechado. Abro a sala de voz quando der o horário 🎮",
    "Quem ficou com o `config.toml` do servidor?",
    "Mandei ontem no privado, procura lá",
    "Alguém sabe por que o jogo trava na tela de carregamento? Já tentei reinstalar duas vezes e nada mudou, continua parando no mesmo lugar.",
    "Eu tive isso, era o driver de vídeo. Atualiza que resolve ||spoiler: resolveu||",
];

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl T {
    /// Accounts for `people` (user name, display name) and a server of the
    /// first one's with everyone in it.
    pub(crate) async fn room(&self, people: &[(&str, &str)]) -> Result<Room> {
        let mut tokens = Vec::new();
        for (name, display) in people {
            tokens.push(self.session(name, display).await?);
        }
        let owner = self.gateway(&tokens[0]).await?;
        owner.next("READY", |_| true).await?;
        let sid = self
            .post(&tokens[0], "/servers", json!({ "name": "Medições" }))
            .await?
            .id("criar servidor")?;
        let server = owner.next("SERVER_UPDATE", |d| d["id"] == sid).await?;
        let text = server["channels"]
            .as_array()
            .and_then(|list| list.iter().find(|c| c["type"] == "text"))
            .and_then(|c| c["id"].as_i64())
            .ok_or("o servidor não tem canal de texto")?;
        let code = self
            .post(&tokens[0], &format!("/servers/{sid}/invites"), Value::Null)
            .await?
            .ok("convite")?["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        for token in &tokens[1..] {
            self.post(
                token,
                &format!("/invites/{}/join", escape(&code)),
                Value::Null,
            )
            .await?
            .ok("entrar")?;
        }
        owner.close().await;
        Ok(Room { tokens, text })
    }

    /// `count` messages in the room's text channel from `tokens` in turn,
    /// so the chat has something to show (each person may send 10 every
    /// 10 s).
    pub(crate) async fn fill_chat(&self, room: &Room, tokens: &[String], count: usize) -> Result {
        let url = format!("/channels/{}/messages", room.text);
        for i in 0..count {
            let token = &tokens[i % tokens.len()];
            // Raw clients cannot encrypt as the apps do: the envelopes have
            // the right shape and size, and show in the apps as refused.
            let content = format!("{} ({i}) {}", CHATTER[i % CHATTER.len()], self.run);
            loop {
                let sent = self
                    .post(token, &url, json!({ "e2e": envelope(&content) }))
                    .await?;
                if sent.status == 429 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                sent.ok("mensagem")?;
                break;
            }
        }
        Ok(())
    }

    /// How long messages take with nothing but the server in the way: the
    /// first person sends `count` of them, one every 1.1 s (the server takes
    /// 10 every 10 s). The times, in ms: the answer to the POST, the
    /// sender's own copy over its connection, the second person's copy,
    /// "typing" over the connection (the same way through the server, but
    /// nothing is written), and the same messages sent over the connection
    /// instead (the sender's copy, the second person's).
    pub(crate) async fn time_messages(&self, room: &Room, count: usize) -> Result<[Vec<f64>; 6]> {
        let a = self.gateway(&room.tokens[0]).await?;
        let b = self.gateway(&room.tokens[1]).await?;
        a.next("READY", |_| true).await?;
        b.next("READY", |_| true).await?;
        let url = format!("/channels/{}/messages", room.text);
        let mut out = [Vec::new(), Vec::new(), Vec::new()];
        for i in 0..count {
            let env = envelope(&format!("cronômetro {i} {}", self.run));
            let started = Instant::now();
            let (posted, mine, theirs) = tokio::join!(
                async {
                    let r = self
                        .post(&room.tokens[0], &url, json!({ "e2e": env }))
                        .await;
                    (r, started.elapsed())
                },
                async {
                    let r = a.next("MESSAGE_CREATE", |d| d["e2e"] == env).await;
                    (r, started.elapsed())
                },
                async {
                    let r = b.next("MESSAGE_CREATE", |d| d["e2e"] == env).await;
                    (r, started.elapsed())
                },
            );
            posted.0?.ok("mensagem")?;
            mine.0?;
            theirs.0?;
            out[0].push(ms(posted.1));
            out[1].push(ms(mine.1));
            out[2].push(ms(theirs.1));
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        let mut typing = Vec::new();
        for _ in 0..12 {
            b.drain("TYPING");
            let started = Instant::now();
            a.op(json!({ "op": "typing", "channelId": room.text }));
            b.next("TYPING", |_| true).await?;
            typing.push(ms(started.elapsed()));
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        // The same messages through the gateway instead of a POST.
        let mut gw = [Vec::new(), Vec::new()];
        for i in 0..count {
            let env = envelope(&format!("pelo gateway {i} {}", self.run));
            let started = Instant::now();
            a.op(json!({
                "op": "message",
                "channelId": room.text,
                "e2e": env,
                "nonce": format!("medir-{i}-{}", self.run),
            }));
            let (mine, theirs) = tokio::join!(
                async {
                    let r = a.next("MESSAGE_CREATE", |d| d["e2e"] == env).await;
                    (r, started.elapsed())
                },
                async {
                    let r = b.next("MESSAGE_CREATE", |d| d["e2e"] == env).await;
                    (r, started.elapsed())
                },
            );
            mine.0?;
            theirs.0?;
            gw[0].push(ms(mine.1));
            gw[1].push(ms(theirs.1));
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        a.close().await;
        b.close().await;
        let [http, echo, other] = out;
        let [gw_echo, gw_other] = gw;
        Ok([http, echo, other, typing, gw_echo, gw_other])
    }

    /// Sends `data` as a file and reads it back. Seconds: to send it, to
    /// the answer of the first byte back, to the whole of it.
    pub(crate) async fn time_file(&self, token: &str, data: &[u8]) -> Result<[f64; 3]> {
        let started = Instant::now();
        let up = self.upload(token, "medida.bin", data).await?;
        let sent = started.elapsed().as_secs_f64();
        let url = up["url"]
            .as_str()
            .ok_or("o envio não tem endereço")?
            .to_owned();
        let started = Instant::now();
        let res = self.download(&url, None).await?;
        let first = started.elapsed().as_secs_f64();
        let mut body = res.bytes_stream();
        let mut got = 0;
        while let Some(chunk) = body.next().await {
            got += chunk?.len();
        }
        check!(got == data.len(), "voltaram {got} de {} bytes", data.len());
        Ok([sent, first, started.elapsed().as_secs_f64()])
    }

    /// Whether the file at `url` is exactly `data` (what an app sent).
    pub(crate) async fn holds(&self, url: &str, data: &[u8]) -> Result<bool> {
        let got = self.download(url, None).await?.bytes().await?;
        Ok(got.as_ref() == data)
    }

    /// The same file the way the app sends and fetches it now: `at_once`
    /// parts going up together, and back down in as many pieces (8 MiB
    /// ranges) together; then the whole of it once more, from the start.
    /// Seconds for each.
    pub(crate) async fn time_file_together(
        &self,
        token: &str,
        data: &[u8],
        at_once: usize,
    ) -> Result<[f64; 3]> {
        let started = Instant::now();
        let start = self
            .post(
                token,
                "/uploads",
                json!({ "name": "juntos.bin", "size": data.len() }),
            )
            .await?;
        let start = start.ok("começar o envio")?.clone();
        let ticket = start["ticket"].as_str().unwrap_or("").to_owned();
        let part = start["partSize"].as_u64().unwrap_or(0) as usize;
        check!(part > 0, "plano do envio: {start}");
        let mut etags = vec![String::new(); data.len().div_ceil(part)];
        let mut going = futures_util::stream::FuturesUnordered::new();
        let mut chunks = data.chunks(part).enumerate();
        loop {
            while going.len() < at_once
                && let Some((i, chunk)) = chunks.next()
            {
                let ticket = &ticket;
                going.push(async move { (i, self.put_part(ticket, i + 1, chunk.to_vec()).await) });
            }
            let Some((i, sent)) = going.next().await else {
                break;
            };
            let sent = sent?;
            etags[i] = sent.ok(&format!("parte {}", i + 1))?["etag"]
                .as_str()
                .unwrap_or("")
                .to_owned();
        }
        let done = self
            .post(
                token,
                "/uploads/complete",
                json!({ "ticket": ticket, "etags": etags }),
            )
            .await?;
        let url = done.ok("juntar as partes")?["url"]
            .as_str()
            .ok_or("o envio não tem endereço")?
            .to_owned();
        let up = started.elapsed().as_secs_f64();

        let started = Instant::now();
        let piece = 8 << 20;
        let ranges: Vec<(usize, usize)> = (0..data.len())
            .step_by(piece)
            .map(|from| (from, (from + piece).min(data.len()) - 1))
            .collect();
        let mut got = vec![Vec::new(); ranges.len()];
        let mut going = futures_util::stream::FuturesUnordered::new();
        let mut todo = ranges.iter().enumerate();
        loop {
            while going.len() < at_once
                && let Some((i, &(from, to))) = todo.next()
            {
                let url = &url;
                going.push(async move {
                    let res = self
                        .download(url, Some(&format!("bytes={from}-{to}")))
                        .await;
                    let bytes = match res {
                        Ok(r) => r.bytes().await.map_err(Into::into),
                        Err(e) => Err(e),
                    };
                    (i, bytes)
                });
            }
            let Some((i, bytes)) = going.next().await else {
                break;
            };
            got[i] = bytes?.to_vec();
        }
        let down = started.elapsed().as_secs_f64();
        check!(got.concat() == data, "os pedaços não refazem o arquivo");

        let started = Instant::now();
        let again = self.download(&url, None).await?.bytes().await?;
        check!(again.len() == data.len(), "voltaram {} bytes", again.len());
        Ok([up, down, started.elapsed().as_secs_f64()])
    }
}

/// One app's WebSocket: events arrive in the background and the test takes
/// them in the order it expects them.
struct Gateway {
    events: Arc<Mutex<Vec<Value>>>,
    arrived: Arc<Notify>,
    send: mpsc::UnboundedSender<Message>,
    /// Writes to the socket; ends once the close frame has gone out.
    writer: tokio::task::JoinHandle<()>,
}

impl Gateway {
    async fn open(base: &str, header: &str, token: &str) -> Result<Gateway> {
        let url = format!("ws{}/gateway", base.strip_prefix("http").unwrap_or(base));
        let mut req = url.into_client_request()?;
        req.headers_mut().insert("x-babu-test", header.parse()?);
        let (ws, _) = tokio_tungstenite::connect_async(req).await?;
        let (mut sink, mut stream) = ws.split();
        let events = Arc::new(Mutex::new(Vec::new()));
        let arrived = Arc::new(Notify::new());
        let (send, mut outbox) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            while let Some(msg) = outbox.recv().await {
                let closing = matches!(msg, Message::Close(_));
                if sink.send(msg).await.is_err() || closing {
                    break;
                }
            }
        });
        {
            let (events, arrived) = (events.clone(), arrived.clone());
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let text = match msg {
                        Message::Text(t) => t.to_string(),
                        Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    if let Ok(ev) = serde_json::from_str::<Value>(&text) {
                        events
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(ev);
                        arrived.notify_waiters();
                    }
                }
            });
        }
        let gw = Gateway {
            events,
            arrived,
            send,
            writer,
        };
        gw.op(json!({ "op": "identify", "token": token }));
        Ok(gw)
    }

    fn op(&self, msg: Value) {
        let _ = self.send.send(Message::text(msg.to_string()));
    }

    /// Drops the events of type `t` that arrived so far, so a wait that
    /// follows sees only new ones.
    fn drain(&self, t: &str) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|e| e["t"] != t);
    }

    /// Whether an event of type `t` whose data passes `pred` has arrived
    /// (and is still untaken), without waiting for one.
    fn seen(&self, t: &str, pred: impl Fn(&Value) -> bool) -> bool {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|e| e["t"] == t && pred(&e["d"]))
    }

    /// The first event of type `t` (not taken yet) whose data passes `pred`.
    async fn next(&self, t: &str, pred: impl Fn(&Value) -> bool) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let notified = self.arrived.notified();
            {
                let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(i) = events.iter().position(|e| e["t"] == t && pred(&e["d"])) {
                    return Ok(events.remove(i)["d"].take());
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                fail!("tempo esgotado esperando {t}");
            }
        }
    }

    /// Says goodbye: done once the close frame has gone out (the writer ends
    /// with it). The published server did not answer it, so a test that
    /// needs the server to have seen the close waits for what it causes.
    async fn close(self) {
        let _ = self.send.send(Message::Close(None));
        let _ = tokio::time::timeout(Duration::from_secs(10), self.writer).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_names_fit_the_servers_rule() {
        let name = run_name();
        assert!(name.len() <= 40, "{name}");
        assert!(
            name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "{name}"
        );
        assert_ne!(run_name(), run_name());
    }
}
