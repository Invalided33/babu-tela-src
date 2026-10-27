//! Replies and headers.

use serde_json::{Value, json};
use worker::{Headers, Response, ResponseBody, Result};

/// The largest JSON body an API call may send: room for the longest
/// message whatever its characters, and for the part list of a file in
/// 10 000 parts.
pub const MAX_BODY: usize = 1 << 20;

/// Headers on every reply. Only the desktop app talks to this server (there
/// is no web version), so there are no CORS headers: no page on any site
/// gets to read a reply. And a browser that opens an upload must take the
/// type it was given instead of guessing one.
pub fn harden(res: &mut Response) {
    let h = res.headers_mut();
    let _ = h.set("X-Content-Type-Options", "nosniff");
}

pub fn json(status: u16, body: &Value) -> Result<Response> {
    // The trailing newline matches the Go server's json.Encoder.
    let mut out = serde_json::to_vec(body).unwrap_or_default();
    out.push(b'\n');
    bytes(status, "application/json; charset=utf-8", out, None)
}

pub fn json_error(status: u16, msg: &str) -> Result<Response> {
    json(status, &json!({ "error": msg }))
}

pub fn bytes(
    status: u16,
    content_type: &str,
    body: Vec<u8>,
    cache: Option<&str>,
) -> Result<Response> {
    let headers = Headers::new();
    headers.set("Content-Type", content_type)?;
    if let Some(cache) = cache {
        headers.set("Cache-Control", cache)?;
    }
    Ok(Response::from_body(ResponseBody::Body(body))?
        .with_status(status)
        .with_headers(headers))
}

pub fn text(status: u16, body: &str) -> Result<Response> {
    bytes(
        status,
        "text/plain; charset=utf-8",
        body.as_bytes().to_vec(),
        None,
    )
}
