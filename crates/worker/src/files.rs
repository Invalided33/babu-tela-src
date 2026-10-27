//! Uploaded files and the installers for automatic updates, kept in R2.
//!
//! The earlier servers wrote these into a folder next to the database. On
//! Cloudflare the object store holds them, so the keys are the same paths
//! without the leading slash ("uploads/<name>", "updates/<name>", and
//! "emoji/<name>" for the copies of the emoji lists).
//!
//! A file goes up in parts (R2's multipart upload), so the app sets no size
//! limit of its own: only R2's, 10 000 parts of up to 95 MiB here (almost a
//! terabyte). Each part is one request that streams straight into the
//! bucket, so neither the Worker nor the chat ever holds a file in memory.
//!
//! - `POST /api/uploads` `{name, size}`, in the chat object: checks the
//!   session, the rate limit and the storage budget, starts the upload and
//!   answers `{ticket, partSize, parts}`.
//! - `PUT /api/uploads/part/{n}`, the part's bytes as the body and the ticket
//!   in `x-upload-ticket`: handled by the Worker itself, which checks the
//!   ticket's signature instead of asking the chat (and waking it) for every
//!   few megabytes. Answers `{etag}`.
//! - `POST /api/uploads/complete` `{ticket, etags}`: joins the parts. The
//!   answer is the attachment to put on a message.
//! - `POST /api/uploads/abort` `{ticket}`: drops an upload nobody is going
//!   to use (a cancelled send).
//!
//! Every file has a row in `uploads`: a message's attachments are checked
//! against it, and the storage budget adds it up.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use worker::{Bucket, Env, Method, Range as R2Range, Request, Response, Result, UploadedPart};

use crate::Hub;
use crate::db::{Row, RowExt};
use crate::http::{MAX_BODY, json, json_error, text};
use crate::model::{is_upload_url, kind_of};
use crate::params;
use crate::state::{State, Trash};
use crate::util::{INTERNAL, base_name, extension, now, random_hex, str, to_lower, truncate};

/// R2 takes parts of at least 5 MiB (all but the last); 8 MiB keeps a part
/// that has to go again cheap.
const MIN_PART: u64 = 8 << 20;

/// Each part is one request, and Workers take request bodies up to 100 MB.
const MAX_PART: u64 = 95 << 20;

/// R2's limit on parts per upload.
const MAX_PARTS: u64 = 10_000;

/// The largest file R2 can take in parts this size.
pub const MAX_FILE: u64 = MAX_PART * MAX_PARTS;

const DAY: i64 = 24 * 3_600_000;

/// A ticket lasts as long as R2 keeps an unfinished upload.
const TICKET_TTL: i64 = 7 * DAY;

/// Where the part requests carry their ticket.
pub const TICKET_HEADER: &str = "x-upload-ticket";

/// The single-request upload of app 0.9.2 and earlier holds the whole file
/// in memory, so it keeps its old cap. Today's app never uses it.
const LEGACY_MAX: usize = 25 << 20;

/// What all the files together may take, unless ARMAZENAMENTO_GB says
/// otherwise: R2's free 10 GB, so the files never cost anything by surprise.
const DEFAULT_BUDGET_GB: f64 = 10.0;

const EXPIRED: &str = "Sessão expirada. Entre novamente.";

/// The R2 bucket binding. Without it there is nowhere to put files.
fn bucket(env: &Env) -> Option<Bucket> {
    env.bucket("FILES").ok()
}

fn no_bucket() -> Result<Response> {
    worker::console_error!("falta o bucket R2 (binding FILES)");
    json_error(503, "O envio de arquivos está indisponível agora.")
}

// ---------------------------------------------------------------- plans and tickets

/// The part size and the number of parts for a file of `size` bytes, or
/// None when it is past what R2 can hold.
pub fn plan(size: u64) -> Option<(u64, u64)> {
    let part = MIN_PART.max(size.div_ceil(MAX_PARTS).next_multiple_of(1 << 20));
    (part <= MAX_PART).then(|| (part, size.div_ceil(part)))
}

/// How long part `n` (counting from 1) of the file is.
pub fn part_len(size: u64, part: u64, n: u64) -> u64 {
    size.saturating_sub(n.saturating_sub(1) * part).min(part)
}

/// What the chat object hands out for one upload. Signed, so the Worker can
/// take its parts on its own.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Ticket {
    /// The stored name: 32 hex characters and maybe an extension.
    pub name: String,
    pub uid: i64,
    /// R2's id for the multipart upload ("" for an empty file).
    pub upload: String,
    pub size: u64,
    pub part: u64,
    pub expires: i64,
}

impl Ticket {
    pub fn parts(&self) -> u64 {
        self.size.div_ceil(self.part.max(1))
    }

    fn key(&self) -> String {
        format!("uploads/{}", self.name)
    }

    fn url(&self) -> String {
        format!("/uploads/{}", self.name)
    }
}

fn signature(pepper: &[u8], body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).expect("qualquer tamanho de chave");
    mac.update(b"envio:");
    mac.update(body.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn issue(pepper: &[u8], t: &Ticket) -> String {
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap_or_default());
    let sig = signature(pepper, &body);
    format!("{body}.{sig}")
}

/// The ticket, when it is one of ours and still good.
pub fn check(pepper: &[u8], ticket: &str, at: i64) -> Option<Ticket> {
    let (body, sig) = ticket.trim().split_once('.')?;
    let want = signature(pepper, body);
    if !bool::from(want.as_bytes().ct_eq(sig.as_bytes())) {
        return None;
    }
    let t: Ticket = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).ok()?).ok()?;
    (t.expires > at && t.part > 0 && is_upload_url(&t.url())).then_some(t)
}

/// A fresh stored name keeping a short, plain extension of the original:
/// it must stay inside what is_upload_url accepts.
fn stored_name(original: &str) -> String {
    let mut ext: String = to_lower(extension(original))
        .chars()
        .filter(|c| *c == '.' || c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    ext.truncate(9);
    if ext.len() <= 1 {
        ext.clear();
    }
    random_hex(16) + &ext
}

/// The name a file is shown with: the base name the app sent.
fn shown_name(given: &str) -> String {
    let name = base_name(given.trim());
    if name.is_empty() || name == "." || name == "/" || name == "\\" {
        return "arquivo".into();
    }
    truncate(name, 255)
}

/// "9,8 GB", the way sizes read here.
pub fn fmt_bytes(n: u64) -> String {
    let gb = n as f64 / 1e9;
    if gb >= 100.0 {
        format!("{gb:.0} GB")
    } else if gb >= 1.0 {
        format!("{gb:.1} GB").replace('.', ",")
    } else {
        format!("{:.0} MB", (n as f64 / 1e6).max(1.0))
    }
}

// ---------------------------------------------------------------- the budget

/// What all the files may add up to, None for no limit
/// (ARMAZENAMENTO_GB=0). Counted in GB of a billion bytes, like the bill.
fn budget(st: &State) -> Option<u64> {
    let raw = st.var("ARMAZENAMENTO_GB");
    let raw = raw.trim();
    let gb = if raw.is_empty() {
        DEFAULT_BUDGET_GB
    } else {
        raw.replace(',', ".")
            .parse::<f64>()
            .unwrap_or(DEFAULT_BUDGET_GB)
    };
    (gb > 0.0).then_some((gb * 1e9) as u64)
}

/// Bytes kept or on their way (unfinished uploads count, so two at once
/// cannot both fit in the last of the budget).
fn used(st: &State) -> u64 {
    st.db
        .get("SELECT COALESCE(SUM(size), 0) AS n FROM uploads", vec![])
        .ok()
        .flatten()
        .map_or(0, |r| r.int("n").max(0) as u64)
}

fn room_for(st: &State, size: u64) -> std::result::Result<(), String> {
    let Some(max) = budget(st) else {
        return Ok(());
    };
    let used = used(st);
    if used.saturating_add(size) <= max {
        return Ok(());
    }
    Err(format!(
        "O servidor está sem espaço para arquivos ({} de {} usados).",
        fmt_bytes(used),
        fmt_bytes(max)
    ))
}

// ---------------------------------------------------------------- uploads

fn bearer(req: &Request) -> String {
    let v = req
        .headers()
        .get("authorization")
        .ok()
        .flatten()
        .unwrap_or_default();
    v.strip_prefix("Bearer ").unwrap_or(&v).to_owned()
}

async fn read_body(req: &mut Request) -> Option<Value> {
    let raw = req.bytes().await.ok()?;
    if raw.len() > MAX_BODY {
        return None;
    }
    serde_json::from_slice(&raw).ok()
}

/// The signed-in user, or the answer to give instead.
fn session(st: &State, token: &str) -> std::result::Result<i64, (u16, String)> {
    match st.user_for_token(token) {
        Ok(0) => Err((401, EXPIRED.into())),
        Ok(uid) => Ok(uid),
        Err(e) => Err((e.status, e.msg)),
    }
}

/// The attachment a finished upload becomes.
fn attachment(url: &str, row: &Row) -> Value {
    json!({
        "url": url, "name": row.str("original"), "size": row.int("size"), "kind": kind_of(url),
    })
}

/// `POST /api/uploads`: starts one.
pub async fn start(hub: &Hub, req: &mut Request) -> Result<Response> {
    let token = bearer(req);
    let Some(body) = read_body(req).await else {
        return json_error(400, "Pedido inválido");
    };
    let original = shown_name(&str(body.get("name")));
    let size = body.get("size").and_then(Value::as_u64);
    let name = stored_name(&original);
    // The checks and the row go together, with the state taken: two
    // uploads starting at once cannot both squeeze into the last of the
    // budget. Who asks comes first: without a session nothing else is said.
    let (uid, pepper, bucket, part, parts, size) = {
        let mut st = hub.st.borrow_mut();
        let uid = match session(&st, &token) {
            Ok(uid) => uid,
            Err((status, msg)) => return json_error(status, &msg),
        };
        let Some(size) = size else {
            return json_error(400, "Tamanho de arquivo inválido");
        };
        let Some((part, parts)) = plan(size) else {
            return json_error(
                413,
                &format!("Arquivo grande demais: o máximo é {}.", fmt_bytes(MAX_FILE)),
            );
        };
        let Some(bucket) = bucket(&hub.env) else {
            return no_bucket();
        };
        if !st.hit(&format!("upload:{uid}"), 120, 60_000) {
            return json_error(429, "Muitos arquivos seguidos. Espere um pouco.");
        }
        if let Err(msg) = room_for(&st, size) {
            return json_error(507, &msg);
        }
        let pepper = match st.pepper() {
            Ok(p) => p,
            Err(e) => return json_error(e.status, &e.msg),
        };
        if let Err(e) = st.db.run(
            "INSERT INTO uploads(name, user_id, original, size, upload_id, created, done, used)
             VALUES(?, ?, ?, ?, '', ?, 0, 0)",
            params![name, uid, original, size, now()],
        ) {
            return json_error(e.status, &e.msg);
        }
        (uid, pepper, bucket, part, parts, size)
    };
    let upload = if parts == 0 {
        // An empty file has no parts; completing it writes it whole.
        String::new()
    } else {
        match bucket
            .create_multipart_upload(format!("uploads/{name}"))
            .execute()
            .await
        {
            Ok(up) => up.upload_id().await,
            Err(e) => {
                worker::console_error!("r2, começando um envio: {e}");
                forget(hub, &name);
                return json_error(500, INTERNAL);
            }
        }
    };
    {
        let st = hub.st.borrow();
        if let Err(e) = st.db.run(
            "UPDATE uploads SET upload_id = ? WHERE name = ?",
            params![upload, name],
        ) {
            return json_error(e.status, &e.msg);
        }
    }
    let ticket = issue(
        &pepper,
        &Ticket {
            name,
            uid,
            upload,
            size,
            part,
            expires: now() + TICKET_TTL,
        },
    );
    json(
        200,
        &json!({ "ticket": ticket, "partSize": part, "parts": parts }),
    )
}

/// `PUT /api/uploads/part/{n}`, in the Worker: one part, straight into R2.
async fn part(env: &Env, req: &Request, n: &str) -> Result<Response> {
    let Ok(pepper) = env.secret("PEPPER") else {
        worker::console_error!("falta o segredo PEPPER");
        return json_error(500, INTERNAL);
    };
    let given = req
        .headers()
        .get(TICKET_HEADER)
        .ok()
        .flatten()
        .unwrap_or_default();
    let Some(t) = check(pepper.to_string().as_bytes(), &given, now()) else {
        return json_error(403, "Este envio expirou. Tente de novo.");
    };
    let n = n.parse::<u64>().unwrap_or(0);
    if n == 0 || n > t.parts() {
        return json_error(400, "Parte inválida");
    }
    // R2 needs to know a part's length before it starts, and the length
    // also proves the parts add up to the file that was announced.
    let want = part_len(t.size, t.part, n);
    let got = req
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok());
    if got != Some(want) {
        return json_error(400, &format!("A parte {n} deveria ter {want} bytes"));
    }
    let Some(bucket) = bucket(env) else {
        return no_bucket();
    };
    let Some(stream) = req.inner().body() else {
        return json_error(400, "Parte vazia");
    };
    let upload = bucket.resume_multipart_upload(t.key(), t.upload.clone())?;
    match upload.upload_part(n as u16, stream).await {
        Ok(p) => json(200, &json!({ "etag": p.etag() })),
        Err(e) => {
            worker::console_error!("r2, parte {n}: {e}");
            json_error(502, "Uma parte do arquivo não chegou. Tente de novo.")
        }
    }
}

/// The ticket of a request body, checked against the session.
fn ticket_of(
    hub: &Hub,
    token: &str,
    body: &Value,
) -> std::result::Result<(Ticket, Option<Row>), (u16, String)> {
    let st = hub.st.borrow();
    let uid = session(&st, token)?;
    let pepper = st.pepper().map_err(|e| (e.status, e.msg))?;
    let t = check(&pepper, &str(body.get("ticket")), now())
        .filter(|t| t.uid == uid)
        .ok_or((403, "Este envio expirou. Tente de novo.".to_owned()))?;
    let row = st
        .db
        .get(
            "SELECT * FROM uploads WHERE name = ? AND user_id = ?",
            params![t.name, uid],
        )
        .map_err(|e| (e.status, e.msg))?;
    Ok((t, row))
}

/// `POST /api/uploads/complete`: joins the parts into the file.
pub async fn complete(hub: &Hub, req: &mut Request) -> Result<Response> {
    let token = bearer(req);
    let Some(body) = read_body(req).await else {
        return json_error(400, "Pedido inválido");
    };
    let (t, row) = match ticket_of(hub, &token, &body) {
        Ok(found) => found,
        Err((status, msg)) => return json_error(status, &msg),
    };
    let Some(row) = row else {
        return json_error(404, "Este envio não existe mais. Tente de novo.");
    };
    // Asked again after an answer that got lost on the way.
    if row.int("done") == 1 {
        return json(200, &attachment(&t.url(), &row));
    }
    let etags: Vec<String> = body
        .get("etags")
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if etags.len() as u64 != t.parts() {
        return json_error(400, "Faltam partes do arquivo");
    }
    let Some(bucket) = bucket(&hub.env) else {
        return no_bucket();
    };
    let joined = if t.parts() == 0 {
        bucket
            .put(t.key(), Vec::<u8>::new())
            .execute()
            .await
            .map(|o| o.map_or(0, |o| o.size()))
    } else {
        let parts = etags
            .into_iter()
            .enumerate()
            .map(|(i, etag)| UploadedPart::new(i as u16 + 1, etag));
        match bucket.resume_multipart_upload(t.key(), t.upload.clone()) {
            Ok(up) => up.complete(parts).await.map(|o| o.size()),
            Err(e) => Err(e),
        }
    };
    match joined {
        Ok(size) if size == t.size => {}
        Ok(size) => {
            worker::console_error!("arquivo com {size} bytes em vez de {}", t.size);
            let _ = bucket.delete(t.key()).await;
            forget(hub, &t.name);
            return json_error(400, "O arquivo chegou incompleto. Tente de novo.");
        }
        Err(e) => {
            worker::console_error!("r2, juntando as partes: {e}");
            return json_error(
                502,
                "Não consegui juntar as partes do arquivo. Tente de novo.",
            );
        }
    }
    {
        let st = hub.st.borrow();
        if let Err(e) = st.db.run(
            "UPDATE uploads SET done = 1 WHERE name = ?",
            params![t.name],
        ) {
            return json_error(e.status, &e.msg);
        }
    }
    json(200, &attachment(&t.url(), &row))
}

/// `POST /api/uploads/abort`: a send that was cancelled. A file already on
/// a message stays.
pub async fn abort(hub: &Hub, req: &mut Request) -> Result<Response> {
    let token = bearer(req);
    let Some(body) = read_body(req).await else {
        return json_error(400, "Pedido inválido");
    };
    let (t, row) = match ticket_of(hub, &token, &body) {
        Ok(found) => found,
        Err((status, msg)) => return json_error(status, &msg),
    };
    if let Some(row) = row
        && row.int("used") == 0
    {
        empty_trash(
            hub,
            vec![Trash {
                name: t.name,
                upload: t.upload,
                done: row.int("done") == 1,
            }],
        )
        .await;
    }
    json(200, &json!({}))
}

/// Forgets an upload's row (one that never got its file).
fn forget(hub: &Hub, name: &str) {
    let st = hub.st.borrow();
    let _ = st
        .db
        .run("DELETE FROM uploads WHERE name = ?", params![name]);
}

/// Takes files out of R2, and their rows once they are gone.
pub async fn empty_trash(hub: &Hub, trash: Vec<Trash>) {
    if trash.is_empty() {
        return;
    }
    let Some(bucket) = bucket(&hub.env) else {
        return;
    };
    for item in trash {
        let key = format!("uploads/{}", item.name);
        let gone = if item.done || item.upload.is_empty() {
            bucket.delete(&key).await.is_ok()
        } else {
            // An upload R2 already dropped (or that got finished after all)
            // fails to abort, and is just as gone.
            if let Ok(up) = bucket.resume_multipart_upload(key, item.upload) {
                let _ = up.abort().await;
            }
            true
        };
        if gone {
            forget(hub, &item.name);
        }
    }
}

/// The upload of app 0.9.2 and earlier: the "file" field of a multipart
/// form, in one request.
pub async fn upload(hub: &Hub, req: &mut Request) -> Result<Response> {
    let token = bearer(req);
    // The session check and the rate limit are quick, so they run with the
    // state taken; reading the file itself then waits on nothing else.
    let uid = {
        let mut st = hub.st.borrow_mut();
        let uid = match session(&st, &token) {
            Ok(uid) => uid,
            Err((status, msg)) => return json_error(status, &msg),
        };
        if !st.hit(&format!("upload:{uid}"), 120, 60_000) {
            return json_error(429, "Muitos arquivos seguidos. Espere um pouco.");
        }
        uid
    };
    let Some(bucket) = bucket(&hub.env) else {
        return no_bucket();
    };
    let Ok(form) = req.form_data().await else {
        return json_error(400, "Falha no envio");
    };
    // A form may carry several "file" parts; take the first real file with a
    // name, like the earlier servers did.
    let found = form
        .get_all("file")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| match e {
            worker::FormEntry::File(f) if !f.name().is_empty() => Some(f),
            _ => None,
        })
        .next();
    let Some(file) = found else {
        return json_error(400, "Nenhum arquivo enviado");
    };
    let too_big =
        "Arquivo maior que 25 MB. Atualize o app para enviar arquivos de qualquer tamanho.";
    if file.size() > LEGACY_MAX {
        return json_error(400, too_big);
    }
    let original = shown_name(&file.name());
    let name = stored_name(&original);
    let Ok(data) = file.bytes().await else {
        return json_error(400, "Falha no envio");
    };
    if data.len() > LEGACY_MAX {
        return json_error(400, too_big);
    }
    let size = data.len() as u64;
    {
        let st = hub.st.borrow();
        if let Err(msg) = room_for(&st, size) {
            return json_error(507, &msg);
        }
        if let Err(e) = st.db.run(
            "INSERT INTO uploads(name, user_id, original, size, upload_id, created, done, used)
             VALUES(?, ?, ?, ?, '', ?, 0, 0)",
            params![name, uid, original, size, now()],
        ) {
            return json_error(e.status, &e.msg);
        }
    }
    if bucket
        .put(format!("uploads/{name}"), data)
        .execute()
        .await
        .is_err()
    {
        forget(hub, &name);
        return json_error(500, INTERNAL);
    }
    let row = {
        let st = hub.st.borrow();
        let _ = st
            .db
            .run("UPDATE uploads SET done = 1 WHERE name = ?", params![name]);
        st.db
            .get("SELECT * FROM uploads WHERE name = ?", params![name])
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    json(200, &attachment(&format!("/uploads/{name}"), &row))
}

/// The stored name of an attachment, when it is a finished upload: what a
/// message or an avatar may point to.
pub fn finished(st: &State, url: &str) -> crate::util::ApiResult<Option<Row>> {
    if !is_upload_url(url) {
        return Ok(None);
    }
    st.db.get(
        "SELECT * FROM uploads WHERE name = ? AND done = 1",
        params![&url["/uploads/".len()..]],
    )
}

/// Removes an uploaded file ("/uploads/<name>") and its row.
pub async fn delete(hub: &Hub, url: &str) {
    if !is_upload_url(url) {
        return;
    }
    empty_trash(
        hub,
        vec![Trash {
            name: url["/uploads/".len()..].to_owned(),
            upload: String::new(),
            done: true,
        }],
    )
    .await;
}

// ---------------------------------------------------------------- reading

/// What the Worker answers on its own, without waking the chat: reading
/// files, and the parts of an upload. None for everything else.
pub async fn stateless(req: &Request, env: &Env) -> Option<Result<Response>> {
    let path = req.path();
    let method = req.method();
    if matches!(method, Method::Get | Method::Head) {
        if let Some(name) = path.strip_prefix("/uploads/") {
            return Some(serve(env, name, req).await);
        }
        if let Some(name) = path.strip_prefix("/updates/") {
            return Some(serve_update(env, name, req).await);
        }
        if let Some(name) = path.strip_prefix("/emoji/") {
            return Some(serve_emoji(env, name, req).await);
        }
    }
    if method == Method::Put
        && let Some(n) = path.strip_prefix("/api/uploads/part/")
    {
        return Some(part(env, req, n).await);
    }
    None
}

/// Serves an uploaded file. Only known media types render inline;
/// everything else downloads, so an uploaded HTML or SVG file can never run
/// in the app's origin.
async fn serve(env: &Env, name: &str, req: &Request) -> Result<Response> {
    let url = format!("/uploads/{name}");
    if !is_upload_url(&url) {
        return text(404, "404 page not found\n");
    }
    let disposition = (kind_of(&url) == "file").then_some("attachment");
    object(
        env,
        &format!("uploads/{name}"),
        req,
        Some("public, max-age=2592000"),
        disposition,
    )
    .await
}

/// The installers and latest.json of automatic updates.
async fn serve_update(env: &Env, name: &str, req: &Request) -> Result<Response> {
    // Only plain names, so nothing can climb out of the folder.
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return text(404, "404 page not found\n");
    }
    object(env, &format!("updates/{name}"), req, None, None).await
}

/// The copies of the official emoji lists the apps read instead of asking
/// unicode.org and GitHub (babutela_buildinfo::emoji), which `cargo xtask
/// emoji` keeps up to date. Only those three names.
async fn serve_emoji(env: &Env, name: &str, req: &Request) -> Result<Response> {
    if !babutela_buildinfo::emoji::is_file(name) {
        return text(404, "404 page not found\n");
    }
    object(env, &format!("emoji/{name}"), req, None, None).await
}

async fn object(
    env: &Env,
    key: &str,
    req: &Request,
    cache: Option<&str>,
    disposition: Option<&str>,
) -> Result<Response> {
    let Some(bucket) = bucket(env) else {
        return text(404, "404 page not found\n");
    };
    let header = |name: &str| req.headers().get(name).ok().flatten();
    let head_only = req.method() == Method::Head;

    // Players ask for pieces of a video or an audio file while seeking, and
    // a download that stopped halfway asks for the rest.
    let wanted = header("range").as_deref().and_then(parse_range);
    let found = if head_only {
        bucket.head(key).await
    } else {
        let mut get = bucket.get(key);
        if let Some(range) = wanted.clone() {
            get = get.range(range);
        }
        get.execute().await
    };
    let Ok(Some(found)) = found else {
        return text(404, "404 page not found\n");
    };

    let tag = found.http_etag();
    let mut res = match (head_only, header("if-none-match")) {
        // The copy the browser already has is still good.
        (_, Some(before)) if before == tag => Response::empty()?.with_status(304),
        (true, _) => Response::empty()?,
        _ => {
            let Some(body) = found.body() else {
                return text(404, "404 page not found\n");
            };
            let status = if wanted.is_some() { 206 } else { 200 };
            Response::from_body(body.response_body()?)?.with_status(status)
        }
    };

    // Which slice went out, for the 206 answer. Worked out from what was
    // asked and the object's size: reading it back from the object
    // (`found.range()`) panics in the SDK.
    let span = wanted
        .filter(|_| res.status_code() == 206)
        .map(|asked| content_range(&asked, found.size()));

    let h = res.headers_mut();
    let _ = h.set("ETag", &tag);
    let _ = h.set("Accept-Ranges", "bytes");
    let _ = h.set("Content-Type", content_type(key));
    if let Some(cache) = cache {
        let _ = h.set("Cache-Control", cache);
    }
    if let Some(d) = disposition {
        let _ = h.set("Content-Disposition", d);
    }
    if let Some(span) = span {
        let _ = h.set("Content-Range", &span);
    }
    // A HEAD answer has no body to tell the size by.
    if head_only {
        let _ = h.set("Content-Length", &found.size().to_string());
    }
    Ok(res)
}

/// The Content-Range of a 206: the slice R2 sends for `asked` (it stops at
/// the end of the object).
fn content_range(asked: &R2Range, total: u64) -> String {
    let (start, len) = match *asked {
        R2Range::OffsetWithLength { offset, length } => {
            (offset, length.min(total.saturating_sub(offset)))
        }
        R2Range::OffsetToEnd { offset } => (offset, total.saturating_sub(offset)),
        R2Range::Prefix { length } => (0, length.min(total)),
        R2Range::Suffix { suffix } => {
            let len = suffix.min(total);
            (total - len, len)
        }
    };
    format!("bytes {start}-{}/{total}", (start + len).saturating_sub(1))
}

/// A single "bytes=..." range, the only form players use.
fn parse_range(value: &str) -> Option<R2Range> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (from, to) = spec.split_once('-')?;
    let (from, to) = (from.trim(), to.trim());
    match (from.parse::<u64>(), to.parse::<u64>()) {
        (Ok(start), Ok(end)) if end >= start => Some(R2Range::OffsetWithLength {
            offset: start,
            length: end - start + 1,
        }),
        (Ok(start), Err(_)) if to.is_empty() => Some(R2Range::OffsetToEnd { offset: start }),
        (Err(_), Ok(last)) if from.is_empty() && last > 0 => Some(R2Range::Suffix { suffix: last }),
        _ => None,
    }
}

/// A content type for the extensions uploads can have.
fn content_type(name: &str) -> &'static str {
    match extension(name).trim_start_matches('.') {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "wmv" => "video/x-ms-wmv",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "weba" => "audio/webm",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_split_into_parts_r2_accepts() {
        assert_eq!(plan(0), Some((MIN_PART, 0)));
        assert_eq!(plan(1), Some((MIN_PART, 1)));
        assert_eq!(plan(MIN_PART), Some((MIN_PART, 1)));
        assert_eq!(plan(MIN_PART + 1), Some((MIN_PART, 2)));
        // Past 10 000 parts of 8 MiB, the parts grow (whole MiB each).
        let big = MIN_PART * MAX_PARTS + 1;
        let (part, parts) = plan(big).unwrap();
        assert_eq!(part, 9 << 20);
        assert!(parts <= MAX_PARTS);
        assert_eq!(plan(MAX_FILE), Some((MAX_PART, MAX_PARTS)));
        assert_eq!(plan(MAX_FILE + 1), None);
        // The parts add up to the file, and only the last one is short.
        let size = 3 * MIN_PART + 12_345;
        let (part, parts) = plan(size).unwrap();
        let lens: Vec<u64> = (1..=parts).map(|n| part_len(size, part, n)).collect();
        assert_eq!(lens, [MIN_PART, MIN_PART, MIN_PART, 12_345]);
        assert_eq!(lens.iter().sum::<u64>(), size);
    }

    #[test]
    fn tickets_only_work_signed_and_in_time() {
        let t = Ticket {
            name: format!("{}.mp4", "a1".repeat(16)),
            uid: 7,
            upload: "id+/=".into(),
            size: 20 << 20,
            part: MIN_PART,
            expires: 1_000,
        };
        let ticket = issue(b"pimenta", &t);
        assert_eq!(check(b"pimenta", &ticket, 999), Some(t.clone()));
        assert_eq!(check(b"pimenta", &ticket, 1_000), None, "vencido");
        assert_eq!(check(b"outra", &ticket, 999), None, "outra chave");
        // Any change to the body breaks the signature.
        let (body, sig) = ticket.split_once('.').unwrap();
        let mut forged = t.clone();
        forged.uid = 8;
        let forged_body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_ne!(forged_body, body);
        assert_eq!(
            check(b"pimenta", &format!("{forged_body}.{sig}"), 999),
            None
        );
        assert_eq!(check(b"pimenta", "lixo", 999), None);
        assert_eq!(t.parts(), 3);
    }

    #[test]
    fn stored_names_stay_plain() {
        let n = stored_name("Férias 2026.MP4");
        assert!(n.ends_with(".mp4") && n.len() == 36, "{n}");
        assert!(is_upload_url(&format!("/uploads/{n}")));
        let n = stored_name("sem-extensao");
        assert_eq!(n.len(), 32);
        let n = stored_name("estranho.ção");
        assert!(is_upload_url(&format!("/uploads/{n}")), "{n}");
        assert_eq!(shown_name("C:\\fotos\\praia.jpg"), "praia.jpg");
        assert_eq!(shown_name("  "), "arquivo");
        assert_eq!(fmt_bytes(9_800_000_000), "9,8 GB");
        assert_eq!(fmt_bytes(MAX_FILE), "996 GB");
        assert_eq!(fmt_bytes(25_000_000), "25 MB");
    }

    #[test]
    fn ranges_answer_with_the_slice_sent() {
        let slice = |v: &str| content_range(&parse_range(v).unwrap(), 1000);
        assert_eq!(slice("bytes=0-99"), "bytes 0-99/1000");
        assert_eq!(slice("bytes=900-"), "bytes 900-999/1000");
        // Past the end, R2 stops at the end.
        assert_eq!(slice("bytes=990-2000"), "bytes 990-999/1000");
        assert_eq!(slice("bytes=-100"), "bytes 900-999/1000");
        assert_eq!(slice("bytes=-5000"), "bytes 0-999/1000");
        assert!(parse_range("bytes=0-1,5-6").is_none());
        assert!(parse_range("items=0-1").is_none());
    }
}
