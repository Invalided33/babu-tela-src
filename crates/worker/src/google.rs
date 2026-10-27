//! Signing in with Google, the only way into the chat.
//!
//! The app does the part a browser is needed for: it opens Google's page
//! with a redirect to a port of its own on 127.0.0.1 (Google's "desktop
//! app" clients take any loopback port) and a PKCE challenge, and gets back
//! a one-time code. It hands that code here, with the challenge's verifier,
//! and this exchanges it at Google's token endpoint with the client secret,
//! which never leaves the Worker. The ID token that comes back arrives
//! straight from Google over HTTPS, in answer to a request the secret
//! authenticated, so its claims are read without checking its signature, as
//! Google's OpenID Connect guide allows for exactly this case.
//!
//! A Google account is one user, found by its `sub` (Google's id for it,
//! which never changes; the e-mail can). The first sign-in makes the user.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use worker::wasm_bindgen::JsValue;
use worker::{Fetch, Headers, Method, Request, RequestInit, Response, Result};

use crate::Hub;
use crate::db::RowExt;
use crate::http::{MAX_BODY, json as json_response, json_error};
use crate::model::COLORS;
use crate::params;
use crate::routes::{DISPLAY_LEN, Req, valid_username};
use crate::state::State;
use crate::util::{ApiError, ApiResult, fail, now, pick, random_hex, str, truncate};

/// Google's token endpoint. GOOGLE_TOKEN_URL replaces it, so a test run can
/// put a fake Google of its own in its place.
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const NOT_SET: &str = "O login com o Google ainda não foi configurado neste servidor.";
const MISCONFIGURED: &str = "O login com o Google está mal configurado no servidor.";
const UNEXPECTED: &str = "O Google não respondeu como esperado. Tente de novo.";

/// User names made from an e-mail stop here, leaving room for a number.
const BASE_LEN: usize = 24;

/// The OAuth client the Worker's secrets name.
struct Config {
    client_id: String,
    secret: String,
    token_url: String,
}

impl State {
    fn google(&self) -> Option<Config> {
        let client_id = self.var("GOOGLE_CLIENT_ID");
        let secret = self.var("GOOGLE_CLIENT_SECRET");
        if client_id.is_empty() || secret.is_empty() {
            return None;
        }
        let token_url = Some(self.var("GOOGLE_TOKEN_URL"))
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| TOKEN_URL.into());
        Some(Config {
            client_id,
            secret,
            token_url,
        })
    }
}

/// `GET /api/auth/google`: what the app needs to open Google's page. The
/// client id is public (it shows in that page's address); the secret stays.
pub fn config(st: &mut State, _: &mut Req) -> ApiResult {
    match st.google() {
        Some(cfg) => Ok(json!({ "clientId": cfg.client_id })),
        None => fail(503, NOT_SET),
    }
}

/// Who Google says signed in.
#[derive(Debug, PartialEq)]
pub struct Identity {
    pub sub: String,
    /// Only when Google has verified it; "" otherwise.
    pub email: String,
    pub name: String,
}

/// `POST /api/auth/google {code, verifier, redirectUri}`: signs in (a new
/// session, and the user too the first time).
pub async fn sign_in(hub: &Hub, req: &mut Request) -> Result<Response> {
    let raw = req.bytes().await.unwrap_or_default();
    if raw.len() > MAX_BODY {
        return json_error(400, "Pedido inválido");
    }
    let body: Map<String, Value> = serde_json::from_slice(&raw).unwrap_or_default();
    let code = str(body.get("code"));
    let verifier = str(body.get("verifier"));
    let redirect = str(body.get("redirectUri"));
    if code.is_empty() || code.len() > 2048 || !pkce_verifier(&verifier) || !loopback(&redirect) {
        return json_error(400, "Pedido inválido");
    }
    let Some(cfg) = hub.st.borrow().google() else {
        return json_error(503, NOT_SET);
    };
    let who = match exchange(&cfg, &code, &verifier, &redirect).await {
        Ok(who) => who,
        Err(e) => return json_error(e.status, &e.msg),
    };
    let out = hub.st.borrow_mut().google_user(&who);
    match out {
        Ok(v) => json_response(200, &v),
        Err(e) => json_error(e.status, &e.msg),
    }
}

/// The code, its verifier and the redirect it was given for, traded at
/// Google's token endpoint for who signed in.
async fn exchange(cfg: &Config, code: &str, verifier: &str, redirect: &str) -> ApiResult<Identity> {
    let form = form_encode(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect),
        ("client_id", &cfg.client_id),
        ("client_secret", &cfg.secret),
    ]);
    let unexpected = |e: worker::Error| {
        worker::console_error!("Google: {e}");
        ApiError {
            status: 502,
            msg: UNEXPECTED.into(),
        }
    };
    let headers = Headers::new();
    headers
        .set("Content-Type", "application/x-www-form-urlencoded")
        .map_err(unexpected)?;
    headers
        .set("Accept", "application/json")
        .map_err(unexpected)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&form)));
    let request = Request::new_with_init(&cfg.token_url, &init).map_err(unexpected)?;
    let mut res = Fetch::Request(request).send().await.map_err(unexpected)?;
    let status = res.status_code();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if status != 200 {
        return Err(token_error(status, &body));
    }
    claims(
        body["id_token"].as_str().unwrap_or_default(),
        &cfg.client_id,
        now() / 1000,
    )
}

/// What a refusal from the token endpoint means for the person signing in.
fn token_error(status: u16, body: &Value) -> ApiError {
    let error = body["error"].as_str().unwrap_or_default();
    let (status, msg) = match error {
        // The code was used already, expired (it lasts minutes) or is not one.
        "invalid_grant" => (
            401,
            "O Google não aceitou esse login (ele pode ter expirado). Tente de novo.",
        ),
        "invalid_client" | "unauthorized_client" => {
            worker::console_error!(
                "Google recusou o cliente ({error}): confira GOOGLE_CLIENT_ID e GOOGLE_CLIENT_SECRET"
            );
            (500, MISCONFIGURED)
        }
        "invalid_request" => (400, "O Google recusou o pedido de login."),
        _ => {
            worker::console_error!("Google respondeu {status}: {body}");
            (502, UNEXPECTED)
        }
    };
    ApiError {
        status,
        msg: msg.into(),
    }
}

/// The claims of an ID token from the token endpoint, if it is for this
/// client, from Google, not expired, and names someone.
pub fn claims(id_token: &str, client_id: &str, now_secs: i64) -> ApiResult<Identity> {
    let bad = || ApiError {
        status: 401,
        msg: "O Google mandou uma resposta que não deu para usar. Tente de novo.".into(),
    };
    let payload = id_token.split('.').nth(1).ok_or_else(bad)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| bad())?;
    let c: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    let for_us = match &c["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(list) => list.iter().any(|aud| aud.as_str() == Some(client_id)),
        _ => false,
    };
    let from_google = matches!(
        c["iss"].as_str(),
        Some("https://accounts.google.com" | "accounts.google.com")
    );
    let exp = c["exp"]
        .as_i64()
        .or_else(|| c["exp"].as_f64().map(|f| f as i64))
        .unwrap_or(0);
    let sub = c["sub"].as_str().unwrap_or_default();
    // A few minutes of slack for a clock that is a little off.
    if !for_us || !from_google || exp + 300 < now_secs || sub.is_empty() || sub.len() > 255 {
        return Err(bad());
    }
    let verified = c["email_verified"] == true || c["email_verified"] == "true";
    let email = if verified {
        c["email"].as_str().unwrap_or_default().trim().to_owned()
    } else {
        String::new()
    };
    let name = c["name"]
        .as_str()
        .or_else(|| c["given_name"].as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    Ok(Identity {
        sub: sub.to_owned(),
        email,
        name,
    })
}

impl State {
    /// A session for whoever signed in; their user is made the first time.
    fn google_user(&mut self, who: &Identity) -> ApiResult {
        if let Some(u) = self.db.get(
            "SELECT id FROM users WHERE google_sub = ?",
            params![who.sub],
        )? {
            let uid = u.int("id");
            if !who.email.is_empty() {
                self.db.run(
                    "UPDATE users SET email = ? WHERE id = ?",
                    params![who.email, uid],
                )?;
            }
            return Ok(json!({ "token": self.new_session(uid)?, "id": uid, "created": false }));
        }
        let username = self.free_username(&username_base(&who.email, &who.name))?;
        let mut display = truncate(&who.name, DISPLAY_LEN);
        if display.is_empty() {
            display.clone_from(&username);
        }
        let uid = self.db.insert(
            "INSERT INTO users(username, display_name, color, created, google_sub, email)
             VALUES(?, ?, ?, ?, ?, ?)",
            params![username, display, pick(COLORS), now(), who.sub, who.email],
        )?;
        Ok(json!({ "token": self.new_session(uid)?, "id": uid, "created": true }))
    }

    /// `base`, or `base` with a number after it, whichever is free.
    fn free_username(&self, base: &str) -> ApiResult<String> {
        for n in 1..100 {
            let name = if n == 1 {
                base.to_owned()
            } else {
                format!("{base}{n}")
            };
            if valid_username(&name)
                && self
                    .db
                    .get("SELECT 1 FROM users WHERE username = ?", params![name])?
                    .is_none()
            {
                return Ok(name);
            }
        }
        Ok(format!("{base}{}", random_hex(3)))
    }
}

/// A user name for someone new, from their e-mail (or, failing that, their
/// name): the server's rule (3 to 32 of a-z, 0-9, `_` and `.`), accents
/// taken off, at most BASE_LEN long.
pub fn username_base(email: &str, name: &str) -> String {
    let clean = |s: &str| {
        let mut out = String::new();
        for c in s.chars().filter_map(fold) {
            // One separator at a time, and none first or last.
            if matches!(c, '.' | '_') && (out.is_empty() || out.ends_with(['.', '_'])) {
                continue;
            }
            out.push(c);
            if out.len() == BASE_LEN {
                break;
            }
        }
        while out.ends_with(['.', '_']) {
            out.pop();
        }
        out
    };
    let local = email.split('@').next().unwrap_or_default();
    [clean(local), clean(name)]
        .into_iter()
        .find(|c| c.len() >= 3)
        .unwrap_or_else(|| "pessoa".into())
}

/// One character of a name as a user name has it, or None to leave it out.
fn fold(c: char) -> Option<char> {
    let c = c.to_lowercase().next().unwrap_or(c);
    Some(match c {
        'a'..='z' | '0'..='9' | '.' | '_' => c,
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        '-' | '+' | ' ' => '_',
        _ => return None,
    })
}

/// The redirect the app listens on: `http://127.0.0.1:<port>` or
/// `http://[::1]:<port>`, nothing else.
fn loopback(uri: &str) -> bool {
    let port = uri
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| uri.strip_prefix("http://[::1]:"));
    port.is_some_and(|p| {
        (1..=5).contains(&p.len())
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n))
    })
}

/// A PKCE code verifier as RFC 7636 has it: 43 to 128 of A-Z, a-z, 0-9 and
/// `-._~`.
fn pkce_verifier(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

/// An `application/x-www-form-urlencoded` body.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let escape = |s: &str| {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", escape(k), escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ID token shaped like Google's (header, claims, signature), with
    /// `claims` as its payload.
    fn id_token(claims: &Value) -> String {
        let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
        format!(
            "{}.{}.assinatura",
            part(&json!({ "alg": "RS256", "typ": "JWT" })),
            part(claims)
        )
    }

    fn google_claims() -> Value {
        json!({
            "iss": "https://accounts.google.com",
            "aud": "cliente.apps.googleusercontent.com",
            "sub": "10769150350006150715113082367",
            "email": "Ana.Souza@gmail.com",
            "email_verified": true,
            "name": "Ana Souza",
            "exp": 2_000_000_000,
        })
    }

    #[test]
    fn a_token_for_this_client_names_its_owner() {
        let who = claims(
            &id_token(&google_claims()),
            "cliente.apps.googleusercontent.com",
            1_999_999_000,
        )
        .unwrap();
        assert_eq!(
            who,
            Identity {
                sub: "10769150350006150715113082367".into(),
                email: "Ana.Souza@gmail.com".into(),
                name: "Ana Souza".into(),
            }
        );
        // Older tokens say the issuer without the scheme, and the
        // verification as text.
        let mut c = google_claims();
        c["iss"] = json!("accounts.google.com");
        c["email_verified"] = json!("true");
        let who = claims(&id_token(&c), "cliente.apps.googleusercontent.com", 0).unwrap();
        assert_eq!(who.email, "Ana.Souza@gmail.com");
    }

    #[test]
    fn tokens_for_others_old_or_odd_are_refused() {
        let id = "cliente.apps.googleusercontent.com";
        let refused = |c: Value| claims(&id_token(&c), id, 1_999_999_000).unwrap_err().status;
        let mut other = google_claims();
        other["aud"] = json!("outro.apps.googleusercontent.com");
        assert_eq!(refused(other), 401);
        let mut fake = google_claims();
        fake["iss"] = json!("https://accounts.example.com");
        assert_eq!(refused(fake), 401);
        let mut old = google_claims();
        old["exp"] = json!(1_999_990_000);
        assert_eq!(refused(old), 401);
        let mut nobody = google_claims();
        nobody["sub"] = json!("");
        assert_eq!(refused(nobody), 401);
        assert!(claims("não-é-um-token", id, 0).is_err());
        assert!(claims("a.%%%.c", id, 0).is_err());
        // An e-mail Google has not verified is not kept.
        let mut unverified = google_claims();
        unverified["email_verified"] = json!(false);
        let who = claims(&id_token(&unverified), id, 0).unwrap();
        assert_eq!(who.email, "");
        assert_eq!(who.sub, "10769150350006150715113082367");
    }

    #[test]
    fn user_names_come_from_the_email() {
        assert_eq!(username_base("Ana.Souza@gmail.com", "Ana"), "ana.souza");
        assert_eq!(username_base("joão+chat@x.com", ""), "joao_chat");
        assert_eq!(username_base("..a--b__c..@x.com", ""), "a_b_c");
        // Too short (or no e-mail): the name, then a default.
        assert_eq!(username_base("ab@x.com", "Lucas Bezerra"), "lucas_bezerra");
        assert_eq!(username_base("", "Zé"), "pessoa");
        assert_eq!(username_base("日本@x.com", "李"), "pessoa");
        let long = username_base("um.nome.de.email.muito.comprido.mesmo@x.com", "");
        assert!(long.len() <= BASE_LEN, "{long}");
        assert!(!long.ends_with('.'), "{long}");
        for name in [
            "ana.souza",
            "joao_chat",
            "a_b_c",
            "lucas_bezerra",
            "pessoa",
            &long,
        ] {
            assert!(valid_username(name), "{name}");
            assert!(valid_username(&format!("{name}99")), "{name}");
            assert!(valid_username(&format!("{name}abcdef")), "{name}");
        }
    }

    #[test]
    fn only_the_apps_own_loopback_redirect_is_taken() {
        assert!(loopback("http://127.0.0.1:53682"));
        assert!(loopback("http://[::1]:8080"));
        assert!(!loopback("http://127.0.0.1"));
        assert!(!loopback("http://127.0.0.1:0"));
        assert!(!loopback("http://127.0.0.1:65536"));
        assert!(!loopback("http://127.0.0.1:80/caminho"));
        assert!(!loopback("https://127.0.0.1:80"));
        assert!(!loopback("http://localhost:80"));
        assert!(!loopback("http://127.0.0.1.example.com:80"));
    }

    #[test]
    fn verifiers_and_forms() {
        assert!(pkce_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert!(!pkce_verifier("curto"));
        assert!(!pkce_verifier(&"a".repeat(129)));
        assert!(!pkce_verifier(&format!("{}+", "a".repeat(42))));
        assert_eq!(
            form_encode(&[("code", "4/0A b+c"), ("redirect_uri", "http://127.0.0.1:9")]),
            "code=4%2F0A%20b%2Bc&redirect_uri=http%3A%2F%2F127.0.0.1%3A9"
        );
    }
}
