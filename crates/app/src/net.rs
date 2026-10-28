//! Talking to the server over HTTP: every call the interface makes goes
//! through `Api`, which adds the session token and turns a failure into the
//! message the interface shows.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::model::Attachment;

/// Most files that can go on one message (the server's limit too).
pub const MAX_FILES: usize = 100;

/// Longest a message can be, in characters (the server counts UTF-16
/// units, which only differ for emoji and the like).
pub const MAX_MESSAGE: usize = 100_000;

/// A picture larger than this is not downloaded to be shown in the chat
/// (it still opens in the browser).
pub const PREVIEW_MAX: u64 = 100 * 1024 * 1024;

/// An encrypted file's block, as a size for buffers.
const BLOCK_USIZE: usize = crate::e2e::BLOCK as usize;

/// How a send's uploads are going, read by the interface while they run.
#[derive(Debug, Default)]
pub struct Progress {
    /// Bytes on their way so far, over all the files.
    pub sent: AtomicU64,
    /// Bytes to send in all.
    pub total: AtomicU64,
    /// Set by the person (the X on the bar): stops at the next piece.
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn new(total: u64) -> Arc<Progress> {
        let p = Progress::default();
        p.total.store(total, Ordering::Relaxed);
        Arc::new(p)
    }

    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// From 0 to 1.
    pub fn fraction(&self) -> f32 {
        let total = self.total();
        if total == 0 {
            return 1.0;
        }
        (self.sent() as f64 / total as f64).clamp(0.0, 1.0) as f32
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// What the server answers when an upload starts.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Started {
    ticket: String,
    part_size: u64,
    parts: u64,
}

const CANCELLED: &str = "Envio cancelado.";

/// Tries per part before giving up: a dropped connection, a busy server.
const PART_TRIES: u32 = 5;

/// Parts on their way at once. One at a time, each part waited for R2 to
/// answer before the next one left, and the line sat idle in between:
/// measured on a line that takes 17 MB/s up, a 64 MB file went at 4.4 MB/s
/// one part at a time, 13.8 with four, 16.3 with six and 15.7 with eight.
const PARTS_AT_ONCE: usize = 6;

/// Where one part of a file is.
#[derive(Clone)]
struct Part {
    path: Arc<std::path::PathBuf>,
    offset: u64,
    len: u64,
    /// Encrypted on its way (e2e/file.rs): `offset` and `len` are then in
    /// the encrypted file.
    key: Option<Arc<crate::e2e::FileKey>>,
}

/// The encrypted bytes `start..start + len` of the file at `path`, read and
/// encrypted on a thread of their own.
async fn sealed_piece(
    path: Arc<std::path::PathBuf>,
    key: Arc<crate::e2e::FileKey>,
    start: u64,
    len: u64,
) -> std::io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&*path)?;
        crate::e2e::sealed_range(&key, &mut file, start, len)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// What a file is, from its name: how the app shows it (the server no
/// longer sees the name).
pub fn kind_of_name(name: &str) -> crate::model::Kind {
    use crate::model::Kind;
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => Kind::Image,
        "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "wmv" => Kind::Video,
        "mp3" | "ogg" | "oga" | "opus" | "wav" | "m4a" | "aac" | "flac" | "weba" => Kind::Audio,
        _ => Kind::File,
    }
}

/// A part as a request body read from the file as it leaves, a piece at a
/// time (so the parts going at once hold a few hundred KB, not 8 MiB each).
/// It counts what leaves, for the bar and in `mine` for this part alone (to
/// take back if it has to go again), and stops when the send is cancelled.
fn part_body(part: Part, progress: Arc<Progress>, mine: Arc<AtomicU64>) -> reqwest::Body {
    use tokio::io::AsyncSeekExt;
    const PIECE: u64 = 256 * 1024;
    let stream =
        futures_util::stream::try_unfold((None::<tokio::fs::File>, 0u64), move |(file, done)| {
            let (part, progress, mine) = (part.clone(), progress.clone(), mine.clone());
            async move {
                if done >= part.len {
                    return Ok(None);
                }
                if progress.cancelled() {
                    return Err(std::io::Error::other(CANCELLED));
                }
                let n = PIECE.min(part.len - done);
                if let Some(key) = &part.key {
                    let buf =
                        sealed_piece(part.path.clone(), key.clone(), part.offset + done, n).await?;
                    if buf.len() as u64 != n {
                        return Err(std::io::Error::other("o arquivo mudou durante o envio"));
                    }
                    progress.sent.fetch_add(n, Ordering::Relaxed);
                    mine.fetch_add(n, Ordering::Relaxed);
                    return Ok(Some((Bytes::from(buf), (file, done + n))));
                }
                let mut file = match file {
                    Some(f) => f,
                    None => {
                        let mut f = tokio::fs::File::open(&*part.path).await?;
                        f.seek(std::io::SeekFrom::Start(part.offset)).await?;
                        f
                    }
                };
                let mut buf = vec![0u8; n as usize];
                file.read_exact(&mut buf).await?;
                progress.sent.fetch_add(n, Ordering::Relaxed);
                mine.fetch_add(n, Ordering::Relaxed);
                Ok(Some((Bytes::from(buf), (Some(file), done + n))))
            }
        });
    reqwest::Body::wrap_stream(stream)
}

/// Long enough for a part on a slow line (16 KB/s), on top of a minute.
fn part_timeout(len: usize) -> Duration {
    Duration::from_secs(60 + len as u64 / (16 * 1024))
}

/// Sends every call to a separate, empty copy of the server's database:
/// `BABUTELA_TEST_HUB=<segredo>:<nome>`. Only works while the server has the
/// TEST_HUB secret, and only for automated tests.
const TEST_HUB_ENV: &str = "BABUTELA_TEST_HUB";
const TEST_HEADER: &str = "x-babu-test";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Put,
    Delete,
}

impl Method {
    fn to_reqwest(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        }
    }
}

/// One call: the method, the path under /api, and the body (`Value::Null`
/// for none).
#[derive(Clone, Debug)]
pub struct Req {
    pub method: Method,
    pub path: String,
    pub body: Value,
}

impl Req {
    pub fn new(method: Method, path: impl Into<String>) -> Req {
        Req {
            method,
            path: path.into(),
            body: Value::Null,
        }
    }

    pub fn with(mut self, body: Value) -> Req {
        self.body = body;
        self
    }

    pub fn get(path: impl Into<String>) -> Req {
        Req::new(Method::Get, path)
    }

    pub fn post(path: impl Into<String>) -> Req {
        Req::new(Method::Post, path)
    }

    pub fn patch(path: impl Into<String>) -> Req {
        Req::new(Method::Patch, path)
    }

    pub fn put(path: impl Into<String>) -> Req {
        Req::new(Method::Put, path)
    }

    pub fn delete(path: impl Into<String>) -> Req {
        Req::new(Method::Delete, path)
    }
}

/// A call that did not work out. `expired` marks the one case the interface
/// treats specially: the session is gone, so sign out instead of complaining.
#[derive(Clone, Debug)]
pub struct ApiError {
    pub message: String,
    pub expired: bool,
}

impl ApiError {
    pub fn msg(message: impl Into<String>) -> ApiError {
        ApiError {
            message: message.into(),
            expired: false,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type ApiResult<T = Value> = Result<T, ApiError>;

/// Shown when the server cannot be reached at all, as opposed to answering
/// with an error.
const UNREACHABLE: &str = "Não foi possível falar com o servidor em ";

/// Shown when the server answers with something other than a message.
const GENERIC: &str = "Algo deu errado";

pub struct Api {
    base: String,
    client: Client,
    /// Separate client with a long timeout, for uploads and downloads.
    slow: Client,
    token: RwLock<Option<String>>,
    /// Set once the server has told us the session is gone, so a burst of
    /// parallel calls only signs out once.
    expired: AtomicBool,
    test: Option<String>,
}

/// Picks the cryptography behind every HTTPS call. Both the HTTP client and
/// the gateway need this done before the first connection, so it happens
/// here, where neither can forget it.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Already installed is just as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Api {
    pub fn new(base: &str) -> Arc<Api> {
        install_crypto_provider();
        let build = |secs: u64| {
            let builder = Client::builder()
                .timeout(Duration::from_secs(secs))
                .connect_timeout(Duration::from_secs(10))
                .user_agent(concat!("BabuTela/", env!("CARGO_PKG_VERSION")));
            match crate::tor::proxy() {
                // Through Tor (tor.rs), whose circuits take longer to open;
                // never around it, so without it there is no client.
                Some(proxy) => builder
                    .proxy(reqwest::Proxy::all(proxy).expect("o proxy do Tor"))
                    .connect_timeout(Duration::from_secs(60))
                    .build()
                    .expect("o cliente HTTP pelo Tor"),
                None => builder.build().unwrap_or_else(|_| Client::new()),
            }
        };
        let test = std::env::var(TEST_HUB_ENV).ok().filter(|v| v.contains(':'));
        Arc::new(Api {
            base: base.trim().trim_end_matches('/').to_owned(),
            client: build(20),
            slow: build(600),
            token: RwLock::new(None),
            expired: AtomicBool::new(false),
            test,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The address of the gateway: the same host, as a WebSocket.
    pub fn gateway_url(&self) -> String {
        let rest = self.base.strip_prefix("http").unwrap_or(&self.base);
        format!("ws{rest}/gateway")
    }

    /// Turns "/uploads/abc.png" into an address that can be fetched.
    pub fn absolute(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_owned();
        }
        format!("{}{}", self.base, path)
    }

    /// The header that sends calls to a test database, when one is set.
    pub fn test_header(&self) -> Option<(&'static str, &str)> {
        self.test.as_deref().map(|v| (TEST_HEADER, v))
    }

    fn tagged(&self, build: RequestBuilder) -> RequestBuilder {
        match self.test_header() {
            Some((name, value)) => build.header(name, value),
            None => build,
        }
    }

    /// The session token. Held by a plain lock, never across an `await`, so
    /// the interface thread can read and change it without a runtime.
    pub fn token(&self) -> Option<String> {
        self.token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_token(&self, token: Option<String>) {
        self.expired.store(false, Ordering::Relaxed);
        *self.token.write().unwrap_or_else(PoisonError::into_inner) = token;
    }

    /// Whether the interface still believes it has a session.
    pub fn signed_in(&self) -> bool {
        self.token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    // ------------------------------------------------------------ calls

    /// Runs one call and gives back the parsed body.
    pub async fn call(&self, req: &Req) -> ApiResult {
        self.send(&self.client, req).await
    }

    /// `call` through a given client (the slow one waits longer).
    async fn send(&self, client: &Client, req: &Req) -> ApiResult {
        let url = format!("{}/api{}", self.base, req.path);
        let mut build = self
            .tagged(client.request(req.method.to_reqwest(), &url))
            // The server only reads a body when this header says JSON, so
            // it goes on every call, as the browser version did.
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(token) = self.token() {
            build = build.bearer_auth(token);
        }
        if !req.body.is_null() {
            build = build.body(serde_json::to_vec(&req.body).unwrap_or_default());
        }
        let res = match build.send().await {
            Ok(res) => res,
            Err(e) if e.is_timeout() => {
                return Err(ApiError::msg("O servidor demorou demais para responder"));
            }
            Err(_) => return Err(ApiError::msg(format!("{UNREACHABLE}{}", self.base))),
        };
        self.finish(res).await
    }

    /// Like `call`, but parses the answer into a type.
    pub async fn call_as<T: DeserializeOwned>(&self, req: &Req) -> ApiResult<T> {
        let value = self.call(req).await?;
        serde_json::from_value(value)
            .map_err(|e| ApiError::msg(format!("Resposta inesperada do servidor: {e}")))
    }

    async fn finish(&self, res: reqwest::Response) -> ApiResult {
        let status = res.status();
        let body: Value = res.json().await.unwrap_or_else(|_| json!({}));
        if status.is_success() {
            return Ok(body);
        }
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or(GENERIC)
            .to_owned();
        if status == StatusCode::UNAUTHORIZED && self.signed_in() {
            // Only the first of a burst reports the session as gone.
            return Err(ApiError {
                message,
                expired: !self.expired.swap(true, Ordering::Relaxed),
            });
        }
        Err(ApiError::msg(message))
    }

    /// Whether the server is answering at all. Used before the sign-in
    /// screen, so nobody types a password into an app that cannot reach it.
    pub async fn alive(&self) -> bool {
        let url = format!("{}/api/version", self.base);
        // Through Tor, the first answer may wait for it to join the network.
        let wait = if crate::tor::active() { 60 } else { 6 };
        matches!(
            self.tagged(self.client.get(url))
                .timeout(Duration::from_secs(wait))
                .send()
                .await,
            Ok(res) if res.status().is_success()
        )
    }

    /// Ends a session on the server. Takes the token instead of reading the
    /// current one, because the interface forgets it straight away.
    pub async fn logout(&self, token: &str) {
        let _ = self
            .tagged(self.client.post(format!("{}/api/logout", self.base)))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(token)
            .send()
            .await;
    }

    // ------------------------------------------------------------ files

    /// Sends one file and gives back the attachment to put on a message.
    ///
    /// Any size: the file goes up in parts the server plans (8 MiB and up),
    /// read from disk as they go, PARTS_AT_ONCE at a time, so a big file
    /// never sits in memory.
    /// `progress` counts the bytes as they leave and can cancel. A part
    /// that fails goes again; a send that stops for good is dropped on the
    /// server, so it leaves nothing behind.
    pub async fn upload(&self, path: &Path, progress: &Arc<Progress>) -> ApiResult<Attachment> {
        self.upload_with(path, progress, None).await
    }

    /// Sends one file of a message, encrypted here as it goes up
    /// (e2e/file.rs): the server gets a neutral name and bytes it cannot
    /// read. The attachment for the envelope (with the file's own name,
    /// size and kind, which only the envelope carries) and its key.
    pub async fn upload_sealed(
        &self,
        path: &Path,
        progress: &Arc<Progress>,
    ) -> ApiResult<(Attachment, crate::e2e::FileKey)> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "arquivo".into());
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|e| ApiError::msg(crate::tf!("net.cant_read", name = name, e = e)))?
            .len();
        let key = crate::e2e::FileKey::new(size);
        let mut a = self.upload_with(path, progress, Some(key.clone())).await?;
        a.kind = kind_of_name(&name);
        a.name = name;
        a.size = i64::try_from(size).unwrap_or(i64::MAX);
        Ok((a, key))
    }

    async fn upload_with(
        &self,
        path: &Path,
        progress: &Arc<Progress>,
        key: Option<crate::e2e::FileKey>,
    ) -> ApiResult<Attachment> {
        let real = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "arquivo".into());
        let unreadable =
            |e: std::io::Error| ApiError::msg(crate::tf!("net.cant_read", name = real, e = e));
        // Opened here only to say early when it cannot be read; each part
        // opens it again for itself.
        let file = tokio::fs::File::open(path).await.map_err(unreadable)?;
        let mut size = file.metadata().await.map_err(unreadable)?.len();
        drop(file);
        // Encrypted, the server sees neither the name nor the kind.
        let name = match &key {
            Some(k) => {
                size = k.sealed_len();
                "arquivo.bin".to_owned()
            }
            None => real.clone(),
        };
        let key = key.map(Arc::new);
        let shared_path = Arc::new(path.to_path_buf());
        let started: Started = self
            .call_as(&Req::post("/uploads").with(json!({ "name": name, "size": size })))
            .await?;
        let ticket = started.ticket.clone();
        let sent = async {
            let mut etags = vec![String::new(); started.parts as usize];
            let mut going = futures_util::stream::FuturesUnordered::new();
            let mut next = 1;
            // A part starts whenever one of the PARTS_AT_ONCE has gone; a
            // part that fails for good ends the send (dropping the others
            // cancels them).
            while next <= started.parts || !going.is_empty() {
                while next <= started.parts && going.len() < PARTS_AT_ONCE {
                    if progress.cancelled() {
                        return Err(ApiError::msg(CANCELLED));
                    }
                    let offset = (next - 1) * started.part_size;
                    let part = Part {
                        path: shared_path.clone(),
                        offset,
                        len: started.part_size.min(size.saturating_sub(offset)),
                        key: key.clone(),
                    };
                    let (n, ticket) = (next, &ticket);
                    going.push(async move { (n, self.put_part(ticket, n, part, progress).await) });
                    next += 1;
                }
                if let Some((n, etag)) = going.next().await {
                    etags[(n - 1) as usize] = etag?;
                }
            }
            // Joining thousands of parts can take R2 a while.
            let done = self
                .send(
                    &self.slow,
                    &Req::post("/uploads/complete")
                        .with(json!({ "ticket": ticket, "etags": etags })),
                )
                .await?;
            serde_json::from_value::<Attachment>(done)
                .map_err(|_| ApiError::msg("O servidor respondeu algo inesperado ao envio"))
        }
        .await;
        if let Err(e) = &sent
            && !e.expired
        {
            let _ = self
                .call(&Req::post("/uploads/abort").with(json!({ "ticket": started.ticket })))
                .await;
        }
        sent
    }

    /// One part, straight to the bucket: tried again after a dropped
    /// connection or a server hiccup, never after a refusal.
    async fn put_part(
        &self,
        ticket: &str,
        n: u64,
        part: Part,
        progress: &Arc<Progress>,
    ) -> ApiResult<String> {
        // What this part has counted on the bar, taken back before it goes
        // again (the other parts keep counting meanwhile).
        let mine = Arc::new(AtomicU64::new(0));
        let take_back = || {
            let counted = mine.swap(0, Ordering::Relaxed);
            progress.sent.fetch_sub(counted, Ordering::Relaxed);
        };
        let mut wait = Duration::from_secs(1);
        for attempt in 1..=PART_TRIES {
            if progress.cancelled() {
                return Err(ApiError::msg(CANCELLED));
            }
            take_back();
            let res = self
                .tagged(self.slow.put(format!("{}/api/uploads/part/{n}", self.base)))
                .header("x-upload-ticket", ticket)
                // Sent as is: R2 has to know a part's length before it starts.
                .header(reqwest::header::CONTENT_LENGTH, part.len)
                .timeout(part_timeout(part.len as usize))
                .body(part_body(part.clone(), progress.clone(), mine.clone()))
                .send()
                .await;
            match res {
                Ok(res) if res.status().is_success() => {
                    let body: Value = res.json().await.unwrap_or_default();
                    if let Some(etag) = body.get("etag").and_then(Value::as_str) {
                        return Ok(etag.to_owned());
                    }
                }
                Ok(res) if res.status().is_client_error() => {
                    take_back();
                    return Err(self.finish(res).await.err().unwrap_or_else(|| {
                        ApiError::msg("O servidor recusou uma parte do arquivo")
                    }));
                }
                _ => {}
            }
            if progress.cancelled() {
                return Err(ApiError::msg(CANCELLED));
            }
            if attempt < PART_TRIES {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
        }
        take_back();
        Err(ApiError::msg(
            "A conexão caiu no meio do envio. Tente de novo.",
        ))
    }

    /// Downloads a file the server is hosting, for pictures. These
    /// addresses are public, so no token goes along. A file past `max`
    /// bytes is left alone.
    pub async fn fetch_bytes(&self, url: &str, max: u64) -> Result<Vec<u8>, String> {
        let res = self
            .tagged(self.slow.get(self.absolute(url)))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("resposta {}", res.status().as_u16()));
        }
        if res.content_length().is_some_and(|n| n > max) {
            return Err("grande demais para mostrar aqui".into());
        }
        let mut out = Vec::new();
        let mut body = res.bytes_stream();
        while let Some(piece) = body.next().await {
            out.extend_from_slice(&piece.map_err(|e| e.to_string())?);
            if out.len() as u64 > max {
                return Err("grande demais para mostrar aqui".into());
            }
        }
        Ok(out)
    }

    /// Saves a hosted file to `path`, straight to disk as it arrives, so it
    /// can be of any size. A message's file is decrypted on the way, block
    /// by block (e2e/file.rs); one that comes changed or cut short leaves
    /// nothing behind.
    pub async fn save_to(&self, url: &str, path: &Path) -> Result<(), String> {
        let saved = self.save_to_inner(url, path).await;
        if saved.is_err() {
            let _ = tokio::fs::remove_file(path).await;
        }
        saved
    }

    async fn save_to_inner(&self, url: &str, path: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let key = crate::e2e::key_for(url);
        let res = self
            .tagged(self.slow.get(self.absolute(url)))
            .timeout(Duration::from_secs(12 * 3600))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("resposta {}", res.status().as_u16()));
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| e.to_string())?;
        let mut body = res.bytes_stream();
        let Some(key) = key else {
            while let Some(piece) = body.next().await {
                file.write_all(&piece.map_err(|e| e.to_string())?)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            return file.flush().await.map_err(|e| e.to_string());
        };
        let tampered = || "o arquivo veio alterado ou incompleto".to_owned();
        let (mut buf, mut block, mut written) = (Vec::new(), 0u64, 0u64);
        while let Some(piece) = body.next().await {
            buf.extend_from_slice(&piece.map_err(|e| e.to_string())?);
            while block < key.blocks() {
                let n =
                    (key.block_len(block) + crate::e2e::SEALED_BLOCK - crate::e2e::BLOCK) as usize;
                if buf.len() < n {
                    break;
                }
                let plain = key.open_block(block, &buf[..n]).ok_or_else(tampered)?;
                let keep = plain.len().min((key.len - written) as usize);
                file.write_all(&plain[..keep])
                    .await
                    .map_err(|e| e.to_string())?;
                written += keep as u64;
                buf.drain(..n);
                block += 1;
            }
        }
        if block != key.blocks() || !buf.is_empty() || written != key.len {
            return Err(tampered());
        }
        file.flush().await.map_err(|e| e.to_string())
    }

    /// Bytes `start..=end` of a hosted file and the file's whole size, for
    /// players that read as they go. A message's file is read in whole
    /// blocks and decrypted (e2e/file.rs): the sizes are its own.
    pub async fn fetch_range(
        &self,
        url: &str,
        start: u64,
        end: u64,
    ) -> Result<(Bytes, u64), String> {
        let Some(key) = crate::e2e::key_for(url) else {
            return self.fetch_raw_range(url, start, end).await;
        };
        if start >= key.len || end < start {
            return Ok((Bytes::new(), start));
        }
        let end = end.min(key.len - 1);
        let blocks = key.blocks_for(start, end + 1);
        let from = crate::e2e::FileKey::sealed_offset(blocks.start);
        let to = crate::e2e::FileKey::sealed_offset(blocks.end).min(key.sealed_len()) - 1;
        let (raw, _) = self.fetch_raw_range(url, from, to).await?;
        let tampered = || "o arquivo veio alterado ou incompleto".to_owned();
        let mut plain = Vec::with_capacity((blocks.end - blocks.start) as usize * BLOCK_USIZE);
        let mut at = 0usize;
        for i in blocks.clone() {
            let n = (key.block_len(i) + crate::e2e::SEALED_BLOCK - crate::e2e::BLOCK) as usize;
            let sealed = raw.get(at..at + n).ok_or_else(tampered)?;
            plain.extend_from_slice(&key.open_block(i, sealed).ok_or_else(tampered)?);
            at += n;
        }
        let off = (start - blocks.start * crate::e2e::BLOCK) as usize;
        let len = (end + 1 - start) as usize;
        let piece = plain.get(off..off + len).ok_or_else(tampered)?;
        Ok((Bytes::copy_from_slice(piece), key.len))
    }

    /// `fetch_range` of the bytes as the server has them.
    async fn fetch_raw_range(
        &self,
        url: &str,
        start: u64,
        end: u64,
    ) -> Result<(Bytes, u64), String> {
        let res = self
            .tagged(self.slow.get(self.absolute(url)))
            .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = res.status();
        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            return Ok((Bytes::new(), start));
        }
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(format!("resposta {}", status.as_u16()));
        }
        let total = res
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit_once('/'))
            .and_then(|(_, n)| n.trim().parse::<u64>().ok())
            .ok_or("resposta sem Content-Range")?;
        Ok((res.bytes().await.map_err(|e| e.to_string())?, total))
    }

    /// The plain HTTP client, for the update check.
    pub fn http(&self) -> &Client {
        &self.slow
    }
}

/// Percent-encodes one path segment, for emoji and invite codes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(b) {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_built_from_the_base() {
        let api = Api::new("https://babutela.workers.dev/");
        assert_eq!(api.base(), "https://babutela.workers.dev");
        assert_eq!(api.gateway_url(), "wss://babutela.workers.dev/gateway");
        assert_eq!(
            api.absolute("/uploads/a.png"),
            "https://babutela.workers.dev/uploads/a.png"
        );
        // An address that is already complete is left alone.
        assert_eq!(api.absolute("https://outro/x.png"), "https://outro/x.png");
        assert_eq!(
            Api::new("http://127.0.0.1:8787").gateway_url(),
            "ws://127.0.0.1:8787/gateway"
        );
    }

    #[test]
    fn the_token_can_be_read_and_changed_without_a_runtime() {
        let api = Api::new("https://x");
        assert!(!api.signed_in());
        api.set_token(Some("abc".into()));
        assert_eq!(api.token().as_deref(), Some("abc"));
        assert!(api.signed_in());
        api.set_token(None);
        assert!(!api.signed_in());
    }

    #[test]
    fn path_segments_are_escaped() {
        assert_eq!(escape("👍"), "%F0%9F%91%8D");
        assert_eq!(escape("abc-1_2.3~"), "abc-1_2.3~");
        assert_eq!(escape("a/b?c d"), "a%2Fb%3Fc%20d");
    }

    /// Reads one HTTP request (head and body) and gives back its head.
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut piece = [0u8; 64 * 1024];
        loop {
            let n = sock.read(&mut piece).await.unwrap();
            assert!(n > 0, "a conexão fechou no meio do pedido");
            buf.extend_from_slice(&piece[..n]);
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().unwrap());
            if buf.len() >= end + 4 + len {
                return head;
            }
        }
    }

    #[tokio::test]
    async fn parts_go_with_their_length_and_again_after_a_failure() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let answers = [
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 14\r\nconnection: close\r\n\r\n{\"etag\":\"abc\"}",
            ];
            let mut heads = Vec::new();
            for answer in answers {
                let (mut sock, _) = listener.accept().await.unwrap();
                heads.push(read_request(&mut sock).await);
                sock.write_all(answer.as_bytes()).await.unwrap();
            }
            heads
        });
        let api = Api::new(&format!("http://{addr}"));
        let progress = Progress::new(300_000);
        // The third part of a file: 300 000 bytes from 100 000 on.
        let path = std::env::temp_dir().join(format!("parte-{}.bin", std::process::id()));
        std::fs::write(&path, vec![7u8; 400_000]).unwrap();
        let part = Part {
            path: Arc::new(path.clone()),
            offset: 100_000,
            len: 300_000,
            key: None,
        };
        let etag = api
            .put_part("bilhete.assinado", 3, part, &progress)
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(etag, "abc");
        // Counted once, though it went twice.
        assert_eq!(progress.sent(), 300_000);
        assert!((progress.fraction() - 1.0).abs() < f32::EPSILON);
        let heads = server.await.unwrap();
        assert_eq!(heads.len(), 2, "tentou de novo depois do erro 500");
        for head in heads {
            assert!(head.starts_with("put /api/uploads/part/3 "), "{head}");
            // R2 needs the length up front: never sent in chunks.
            assert!(head.contains("content-length: 300000"), "{head}");
            assert!(!head.contains("transfer-encoding"), "{head}");
            assert!(head.contains("x-upload-ticket: bilhete.assinado"), "{head}");
        }
    }

    #[tokio::test]
    async fn a_cancelled_send_stops_before_the_next_part() {
        let api = Api::new("http://127.0.0.1:9");
        let progress = Progress::new(10);
        progress.cancel.store(true, Ordering::Relaxed);
        let part = Part {
            path: Arc::new("nao-existe.bin".into()),
            offset: 0,
            len: 10,
            key: None,
        };
        let err = api.put_part("x.y", 1, part, &progress).await.unwrap_err();
        assert_eq!(err.message, CANCELLED);
        assert_eq!(progress.sent(), 0);
    }

    #[test]
    fn requests_describe_themselves() {
        let r = Req::post("/servers").with(json!({"name": "Casa"}));
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.path, "/servers");
        assert_eq!(r.body["name"], "Casa");
        assert!(Req::get("/sessions").body.is_null());
    }
}
