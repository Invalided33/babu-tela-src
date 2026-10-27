//! Signing in with Google, the way Google has desktop apps do it (RFC 8252):
//! the person's own browser opens Google's page, and Google sends it back to
//! a port this app listens on at 127.0.0.1, with a one-time code. PKCE ties
//! that code to this app: the challenge goes to Google with the request,
//! the secret it was made from (the verifier) only to our server, with the
//! code, which trades both at Google for who signed in
//! (crates/worker/src/google.rs). The client secret never comes here.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::net::{Api, ApiError, ApiResult, Req};

/// Google's sign-in page.
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// In the app's test mode, a stand-in for Google's page (it answers like
/// Google, straight back to the app), visited without opening a browser.
const FAKE_ENV: &str = "BABUTELA_GOOGLE_AUTH_URL";

/// How long the browser has to come back with the code.
const WAIT: Duration = Duration::from_secs(10 * 60);

/// Signs in. Gives back the server's answer, with the new session's
/// `token`.
pub async fn sign_in(api: Arc<Api>) -> ApiResult {
    let config = api.call(&Req::get("/auth/google")).await?;
    let client_id = config["clientId"].as_str().unwrap_or_default().to_owned();
    if client_id.is_empty() {
        return Err(ApiError::msg(
            "O servidor não disse como entrar com o Google.",
        ));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| ApiError::msg(format!("Não consegui esperar a resposta do Google: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| ApiError::msg(e.to_string()))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}");
    let verifier = b64url(&random::<32>());
    let state = b64url(&random::<16>());
    let fake = crate::smoke::active()
        .then(|| std::env::var(FAKE_ENV).ok())
        .flatten()
        .filter(|u| !u.is_empty());
    let page = auth_url(
        fake.as_deref().unwrap_or(AUTH_URL),
        &client_id,
        &redirect,
        &challenge(&verifier),
        &state,
    )?;
    match fake {
        // The test's Google sends the "browser" straight back here.
        Some(_) => {
            tokio::spawn(async move {
                if let Err(e) = reqwest::get(page).await {
                    crate::logln!("google de teste: {e}");
                }
            });
        }
        None => open::that_detached(page.as_str())
            .map_err(|e| ApiError::msg(format!("Não consegui abrir o navegador: {e}")))?,
    }
    let code = tokio::time::timeout(WAIT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| ApiError::msg("O login com o Google demorou demais. Tente de novo."))??;
    api.call(&Req::post("/auth/google").with(json!({
        "code": code,
        "verifier": verifier,
        "redirectUri": redirect,
    })))
    .await
}

/// Google's page for this sign-in.
fn auth_url(
    base: &str,
    client_id: &str,
    redirect: &str,
    challenge: &str,
    state: &str,
) -> ApiResult<reqwest::Url> {
    reqwest::Url::parse_with_params(
        base,
        &[
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("response_type", "code"),
            ("scope", "openid email profile"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            // Lets whoever has several Google accounts pick one, every time.
            ("prompt", "select_account"),
        ],
    )
    .map_err(|e| ApiError::msg(e.to_string()))
}

/// Takes the browser's visits until one brings this sign-in's answer, and
/// gives back its code. Anything else (a page asking for its icon, a visit
/// without this sign-in's `state`) gets a 404 and is ignored.
async fn wait_for_code(listener: &TcpListener, state: &str) -> ApiResult<String> {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let Some(target) = read_request(&mut stream).await else {
            continue;
        };
        match answer(&target, state) {
            Some(Ok(code)) => {
                reply(&mut stream, 200, &page(DONE_TITLE, DONE_TEXT)).await;
                return Ok(code);
            }
            Some(Err(why)) => {
                reply(&mut stream, 200, &page("Não deu para entrar", &why)).await;
                return Err(ApiError::msg(why));
            }
            None => reply(&mut stream, 404, &page("Página não encontrada", "")).await,
        }
    }
}

const DONE_TITLE: &str = "Pronto!";
const DONE_TEXT: &str = "Você já pode fechar esta aba e voltar ao Babu Tela.";

/// What a visit to `target` (the path and query of the request) says:
/// None when it is not this sign-in's answer.
fn answer(target: &str, state: &str) -> Option<Result<String, String>> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != "/" {
        return None;
    }
    let get = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if get("state").as_deref() != Some(state) {
        return None;
    }
    if let Some(error) = get("error") {
        return Some(Err(if error == "access_denied" {
            "O login com o Google foi cancelado.".to_owned()
        } else {
            format!("O Google não deixou entrar ({error}).")
        }));
    }
    match get("code") {
        Some(code) if !code.is_empty() => Some(Ok(code)),
        _ => Some(Err("O Google não mandou o código de login.".to_owned())),
    }
}

/// The path and query of one HTTP request, read up to its blank line.
async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    let read = async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
            let n = stream.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .ok()??;
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.lines().next()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    (method == "GET" && target.starts_with('/')).then(|| target.to_owned())
}

async fn reply(stream: &mut TcpStream, status: u16, html: &str) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        html.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(html.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The page the browser shows at the end, in the app's colours.
fn page(title: &str, text: &str) -> String {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        "<!doctype html><html lang=\"pt-BR\"><head><meta charset=\"utf-8\">\
         <title>Babu Tela</title><style>\
         body{{margin:0;height:100vh;display:grid;place-items:center;background:#0f1016;\
         color:#e9eaf2;font:16px/1.5 'Segoe UI',system-ui,sans-serif}}\
         main{{max-width:420px;padding:32px;border-radius:14px;background:#191a24;\
         text-align:center}}h1{{margin:0 0 8px;font-size:22px}}p{{margin:0;color:#a3a6b8}}\
         </style></head><body><main><h1>{}</h1><p>{}</p></main></body></html>",
        escape(title),
        escape(text)
    )
}

/// S256: the challenge Google keeps for `verifier`.
fn challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// Unpredictable bytes, from the same source TLS draws on.
fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    // Never fails on Windows; a sign-in without randomness must not go on.
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut out)
        .expect("sem fonte de números aleatórios");
    out
}

/// Base64 for URLs, without padding (RFC 4648 §5), as PKCE wants it.
fn b64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// What only its owner sees of an account: the e-mail of the Google account
/// it signs in with ("" for the tests' accounts, which have none).
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct Account {
    #[serde(default)]
    pub email: String,
}

impl Account {
    pub fn from_value(v: &Value) -> Account {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636, appendix B.
    #[test]
    fn pkce_matches_the_rfc() {
        let bytes = [
            116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212,
            37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        let verifier = b64url(&bytes);
        assert_eq!(verifier, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            challenge(&verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        // Two draws never match.
        assert_ne!(random::<16>(), random::<16>());
    }

    #[test]
    fn the_page_carries_the_whole_request() {
        let url = auth_url(
            AUTH_URL,
            "123.apps.googleusercontent.com",
            "http://127.0.0.1:5000",
            "desafio",
            "estado",
        )
        .unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(q["client_id"], "123.apps.googleusercontent.com");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5000");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], "openid email profile");
        assert_eq!(q["code_challenge"], "desafio");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "estado");
    }

    #[test]
    fn only_this_sign_ins_answer_counts() {
        assert_eq!(
            answer("/?state=abc&code=4%2F0Ab&scope=email", "abc"),
            Some(Ok("4/0Ab".to_owned()))
        );
        assert_eq!(
            answer("/?state=abc&error=access_denied", "abc"),
            Some(Err("O login com o Google foi cancelado.".to_owned()))
        );
        assert!(matches!(answer("/?state=abc", "abc"), Some(Err(_))));
        // Someone else's state, none at all, or another page: ignored.
        assert_eq!(answer("/?state=xyz&code=1", "abc"), None);
        assert_eq!(answer("/?code=1", "abc"), None);
        assert_eq!(answer("/favicon.ico", "abc"), None);
    }

    /// The listener end to end, with a TCP stream for the browser: a stray
    /// request first, then the answer.
    #[tokio::test]
    async fn the_browser_brings_the_code_back() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            let mut pages = Vec::new();
            for path in ["/favicon.ico", "/?state=s1&code=c%C3%B3digo"] {
                let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
                s.write_all(req.as_bytes()).await.unwrap();
                let mut got = String::new();
                s.read_to_string(&mut got).await.unwrap();
                pages.push(got);
            }
            pages
        });
        let code = tokio::time::timeout(Duration::from_secs(5), wait_for_code(&listener, "s1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(code, "código");
        let pages = browser.await.unwrap();
        assert!(pages[0].starts_with("HTTP/1.1 404"), "{}", pages[0]);
        assert!(pages[1].starts_with("HTTP/1.1 200"), "{}", pages[1]);
        assert!(pages[1].contains(DONE_TEXT));
    }

    #[test]
    fn accounts_read_from_the_server() {
        let a = Account::from_value(&json!({ "email": "a@b.c" }));
        assert_eq!(a.email, "a@b.c");
        assert_eq!(Account::from_value(&Value::Null).email, "");
    }
}
