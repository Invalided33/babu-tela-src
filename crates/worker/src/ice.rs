//! The servers calls use to find a path between two people. Calls are
//! peer to peer: each pair of people is connected directly, and the media
//! never goes through this server.
//!
//! STUN is enough when at least one side can be reached directly. Behind
//! stricter networks the media has to be relayed, which is what TURN does:
//! Cloudflare's own relay (a key kept in the Worker's secrets, from which
//! each user gets credentials that expire), or a TURN server of your own
//! through TURN_URLS and TURN_SECRET.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha1::Sha1;
use worker::wasm_bindgen::JsValue;
use worker::{Fetch, Headers, Method, Request, RequestInit, Response, Result};

use crate::Hub;
use crate::http::{json as json_response, json_error};
use crate::util::now;

/// How long the credentials last. Longer than any call is expected to be.
const TTL: i64 = 12 * 3600;

pub async fn servers(hub: &Hub, req: &Request) -> Result<Response> {
    let token = req
        .headers()
        .get("authorization")
        .ok()
        .flatten()
        .unwrap_or_default();
    let token = token.strip_prefix("Bearer ").unwrap_or(&token).to_owned();
    let (uid, key, key_token, turn, secret, relay_only) = {
        let st = hub.st.borrow();
        let uid = match st.user_for_token(&token) {
            Ok(uid) => uid,
            Err(e) => return json_error(e.status, &e.msg),
        };
        (
            uid,
            st.var("TURN_KEY_ID"),
            st.var("TURN_KEY_TOKEN"),
            st.var("TURN_URLS"),
            st.var("TURN_SECRET"),
            st.var("TURN_RELAY_ONLY") == "1",
        )
    };
    if uid == 0 {
        return json_error(401, "Sessão expirada. Entre novamente.");
    }

    if !key.is_empty() && !key_token.is_empty() {
        match cloudflare(&key, &key_token).await {
            Ok(servers) => {
                return json_response(
                    200,
                    &json!({ "iceServers": servers, "relayOnly": relay_only }),
                );
            }
            // A call with only STUN still works for most people, so a
            // failure here is not worth refusing the call over.
            Err(e) => worker::console_error!("TURN da Cloudflare: {e}"),
        }
    }

    let mut servers = vec![json!({ "urls": ["stun:stun.cloudflare.com:3478"] })];
    let has_turn = !turn.is_empty() && !secret.is_empty();
    if has_turn {
        // The scheme coturn calls "REST API": the user name carries when it
        // expires, and the password is its signature.
        let username = format!("{}:{}", now() / 1000 + TTL, uid);
        let mut mac =
            Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("HMAC aceita qualquer chave");
        mac.update(username.as_bytes());
        let credential = STANDARD.encode(mac.finalize().into_bytes());
        servers.push(json!({
            "urls": turn.split(',').collect::<Vec<_>>(),
            "username": username,
            "credential": credential,
        }));
    }
    json_response(
        200,
        &json!({ "iceServers": servers, "relayOnly": has_turn && relay_only }),
    )
}

/// Asks Cloudflare for credentials that expire.
async fn cloudflare(key: &str, key_token: &str) -> Result<Value> {
    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {key_token}"))?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&json!({ "ttl": TTL }).to_string())));
    let url = format!(
        "https://rtc.live.cloudflare.com/v1/turn/keys/{key}/credentials/generate-ice-servers"
    );
    let mut res = Fetch::Request(Request::new_with_init(&url, &init)?)
        .send()
        .await?;
    if res.status_code() >= 300 {
        return Err(worker::Error::RustError(format!(
            "resposta {}",
            res.status_code()
        )));
    }
    let body: Value = res.json().await?;
    match body.get("iceServers") {
        Some(Value::Array(list)) => Ok(Value::Array(list.clone())),
        // One object instead of a list, as an older version of the API answered.
        Some(obj @ Value::Object(_)) => Ok(json!([obj])),
        _ => Err(worker::Error::RustError("resposta inesperada".into())),
    }
}
