//! Request routing inside the Durable Object.
//!
//! Hand written instead of a framework: the free plan gives each request
//! 10 ms of CPU, and a smaller module also starts faster.

use std::collections::HashMap;

use serde_json::{Map, Value};
use worker::{Method, Request, Response, Result};

use crate::Hub;
use crate::http::{MAX_BODY, harden, json, json_error, text};
use crate::routes::{Handler, ROUTES, Req};
use crate::util::hash_token;
use crate::{files, gateway, google, ice};

pub async fn serve(hub: &Hub, req: Request) -> Result<Response> {
    {
        let st = hub.st.borrow();
        st.refresh();
        gateway::drop_silent(&st);
    }
    let mut res = route(hub, req).await?;
    // A WebSocket handshake (101) is handed to the runtime untouched: its
    // headers are sealed.
    if res.status_code() != 101 {
        harden(&mut res);
    }
    Ok(res)
}

async fn route(hub: &Hub, mut req: Request) -> Result<Response> {
    let method = req.method();
    let path = req.path();

    if path == "/gateway" {
        let upgrade = req
            .headers()
            .get("upgrade")
            .ok()
            .flatten()
            .unwrap_or_default();
        if method != Method::Get || !upgrade.eq_ignore_ascii_case("websocket") {
            return text(400, "este endereço é só para WebSocket\n");
        }
        return gateway::upgrade(&hub.st.borrow(), &req);
    }
    // Uploads talk to R2, so they are handled here too. Their parts never
    // come this far: the Worker takes them (files::stateless).
    if method == Method::Post {
        match path.as_str() {
            "/api/uploads" => return files::start(hub, &mut req).await,
            "/api/uploads/complete" => return files::complete(hub, &mut req).await,
            "/api/uploads/abort" => return files::abort(hub, &mut req).await,
            // The single-request upload of app 0.9.2 and earlier.
            "/api/upload" => return files::upload(hub, &mut req).await,
            _ => {}
        }
    }
    // Asking Cloudflare for call credentials, and Google about a sign-in,
    // need the network, so they are handled here instead of among the plain
    // handlers.
    if path == "/api/ice" && method == Method::Get {
        return ice::servers(hub, &req).await;
    }
    if path == "/api/auth/google" && method == Method::Post {
        return google::sign_in(hub, &mut req).await;
    }
    let verb = method.to_string().to_uppercase();
    for (m, pattern, auth, handler) in ROUTES {
        let Some(params) = match_path(pattern, &path) else {
            continue;
        };
        // A known path with the wrong method answers like an unknown one,
        // the way the earlier servers did.
        if *m != verb && !(*m == "GET" && verb == "HEAD") {
            continue;
        }
        return call(hub, *auth, *handler, params, req).await;
    }
    home(&req, &path)
}

/// Reads the body, checks the session and runs the handler.
async fn call(
    hub: &Hub,
    auth: bool,
    handler: Handler,
    params: HashMap<String, String>,
    mut req: Request,
) -> Result<Response> {
    let is_json = req
        .headers()
        .get("content-type")
        .ok()
        .flatten()
        .is_some_and(|ct| ct.starts_with("application/json"));
    let mut fields = Map::new();
    if is_json {
        let raw = req.bytes().await.unwrap_or_default();
        if raw.len() > MAX_BODY {
            return json_error(400, "Pedido inválido");
        }
        // The first JSON value, like Go's json.Decoder: an empty body is fine.
        match serde_json::Deserializer::from_slice(&raw)
            .into_iter::<Value>()
            .next()
        {
            None => {}
            Some(Err(_)) => return json_error(400, "Pedido inválido"),
            Some(Ok(Value::Object(m))) => fields = m,
            Some(Ok(_)) => {}
        }
    }
    let token = bearer(&req);
    let mut q = Req {
        body: fields,
        params,
        query: parse_query(
            req.url()
                .ok()
                .and_then(|u| u.query().map(str::to_owned))
                .unwrap_or_default()
                .as_str(),
        ),
        uid: 0,
        token_hash: String::new(),
        // The Worker sent it here by the same rule, so a request with the
        // tests' header never reaches the real chat.
        test: crate::on_test_hub(&req, &hub.env),
        remove: Vec::new(),
    };

    // From here on nothing waits on the network, so the chat's state stays
    // taken for one request at a time, like the earlier servers' lock.
    let out = {
        let mut st = hub.st.borrow_mut();
        if auth {
            match st.user_for_token(&token) {
                Ok(0) => return json_error(401, "Sessão expirada. Entre novamente."),
                Ok(uid) => q.uid = uid,
                Err(e) => return json_error(e.status, &e.msg),
            }
            q.token_hash = hash_token(&token);
        }
        let out = handler(&mut st, &mut q);
        st.sweep_sometimes();
        out
    };
    for url in std::mem::take(&mut q.remove) {
        files::delete(hub, &url).await;
    }
    let trash = std::mem::take(&mut hub.st.borrow_mut().trash);
    files::empty_trash(hub, trash).await;
    match out {
        Ok(v) => json(200, &v),
        Err(e) => json_error(e.status, &e.msg),
    }
}

pub(crate) fn bearer(req: &Request) -> String {
    let v = req
        .headers()
        .get("authorization")
        .ok()
        .flatten()
        .unwrap_or_default();
    v.strip_prefix("Bearer ").unwrap_or(&v).to_owned()
}

/// Matches a pattern like "/api/servers/{sid}/channels" against a path,
/// filling the named parts.
fn match_path(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let mut params = HashMap::new();
    let mut want = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return Some(params),
            (Some(w), Some(g)) => {
                if let Some(name) = w.strip_prefix('{').and_then(|w| w.strip_suffix('}')) {
                    if g.is_empty() {
                        return None;
                    }
                    params.insert(name.to_owned(), path_unescape(g));
                } else if w != g {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

/// Percent-decoding for one path segment. Invalid bytes become U+FFFD, as
/// Go's handlers saw them.
fn path_unescape(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3)) {
            (b'%', Some(hex)) if hex.iter().all(u8::is_ascii_hexdigit) => {
                let n = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or("0"), 16);
                out.push(n.unwrap_or(b'%'));
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The query string the way Go's URL.Query() read it: pairs holding a ';' or
/// a broken %-escape are left out.
fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|pair| !pair.is_empty() && !pair.contains(';'))
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((query_unescape(k)?, query_unescape(v)?))
        })
        .collect()
}

/// Go's url.QueryUnescape: "+" is a space and every "%" needs two hex digits.
fn query_unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = b.get(i + 1..i + 3)?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Anything that is not the API. There is no web version: someone who opens
/// the address in a browser is told where to get the app.
fn home(req: &Request, path: &str) -> Result<Response> {
    if path.starts_with("/api/") {
        return json_error(404, "Rota não encontrada");
    }
    if path != "/" {
        return text(404, "404 page not found\n");
    }
    let origin = req
        .url()
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_default();
    text(200, &landing(&origin))
}

fn landing(origin: &str) -> String {
    use babutela_buildinfo::{NAME, VERSION};
    format!(
        "{NAME} {VERSION}\n\n\
         Este endereço é o servidor do {NAME}, um app para Windows.\n\
         Para entrar na conversa, baixe e instale o app:\n\n\
         {origin}/updates/Babu-Tela-Setup.exe\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_points_to_the_installer() {
        let page = landing("https://babutela.example");
        assert!(page.starts_with(&format!("Babu Tela {}", babutela_buildinfo::VERSION)));
        assert!(page.contains("https://babutela.example/updates/Babu-Tela-Setup.exe\n"));
    }

    #[test]
    fn paths_match_their_patterns() {
        let p = match_path("/api/servers/{sid}/channels", "/api/servers/7/channels").unwrap();
        assert_eq!(p["sid"], "7");
        assert!(match_path("/api/servers/{sid}", "/api/servers/").is_none());
        assert!(match_path("/api/servers/{sid}", "/api/servers/7/x").is_none());
        let p = match_path("/api/invites/{code}", "/api/invites/a%20b").unwrap();
        assert_eq!(p["code"], "a b");
    }

    #[test]
    fn queries_read_like_go() {
        assert_eq!(
            parse_query("a=1&b=x+y&c=%41&bad=%zz&semi=1;2"),
            [
                ("a".into(), "1".into()),
                ("b".into(), "x y".into()),
                ("c".into(), "A".into())
            ]
        );
    }
}
