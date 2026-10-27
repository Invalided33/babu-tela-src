//! Tor inside the app (Configurações → Privacidade, "Conectar pelo Tor"):
//! Arti, the Tor Project's own Tor written in Rust, runs in the app itself,
//! so nothing has to be installed. With it on, what the app sends to the
//! server, to GIPHY and for updates leaves through a Tor circuit: the
//! server, and Cloudflare under it, see a Tor exit instead of this PC's IP.
//!
//! - The HTTP clients (net.rs, giphy.rs) go through a SOCKS5 proxy of the
//!   app's own on this PC only (127.0.0.1, a random port, a random user and
//!   password), which hands each connection to Tor (`serve`), names and all:
//!   the names are looked up at the exit, never by this PC.
//! - The gateway (gateway.rs) and the relay's TCP and TLS connections
//!   (voice/relay.rs) open Tor streams directly (`connect`).
//! - Calls and files sent directly go only through the relay over TCP and
//!   TLS (voice/rtc.rs `ice_servers`), since Tor carries no UDP.
//! - Signing in with Google happens in the browser, outside the app and
//!   outside Tor.
//!
//! It is chosen at the start (main.rs), so a change takes a restart.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use arti_client::config::TorClientConfigBuilder;
use arti_client::status::BootstrapStatus;
use arti_client::{DataStream, TorClient, TorClientConfig};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

type Connecting = Pin<Box<dyn Future<Output = io::Result<DataStream>> + Send>>;

struct Tor {
    connect: Box<dyn Fn(String, u16) -> Connecting + Send + Sync>,
    /// Where it stands, as Arti tells it (`start`).
    status: watch::Receiver<Status>,
    /// `socks5h://user:pass@127.0.0.1:port`.
    proxy: String,
}

static TOR: OnceLock<Tor> = OnceLock::new();

/// Where Tor stands, for the interface.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Joining the network: how far (0 to 1).
    Joining(f32),
    Ready,
    /// Stuck: what Arti says is in the way.
    Blocked(String),
}

/// Starts Tor, keeping what it learns of the network in `dir` (the next
/// start takes seconds instead of the first one's tens of seconds). Runs
/// inside the app's runtime.
pub fn start(dir: &Path) -> Result<(), String> {
    let config: TorClientConfig =
        TorClientConfigBuilder::from_directories(dir.join("estado"), dir.join("cache"))
            .build()
            .map_err(|e| e.to_string())?;
    let client = TorClient::builder()
        .config(config)
        .create_unbootstrapped()
        .map_err(|e| e.to_string())?;
    let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;
    let port = std_listener.local_addr().map_err(|e| e.to_string())?.port();
    let listener = TcpListener::from_std(std_listener).map_err(|e| e.to_string())?;
    // Without the system's randomness the proxy would be open to any
    // program on this PC: better not to start at all.
    let user = hex::encode(random::<12>()?);
    let pass = hex::encode(random::<24>()?);
    let proxy = format!("socks5h://{user}:{pass}@127.0.0.1:{port}");
    // Where it stands, each time Arti says it changed.
    let (tell, status) = watch::channel(status_of(&client.bootstrap_status()));
    let mut events = client.bootstrap_events();
    tokio::spawn(async move {
        // In the log at each step, with what Arti says of it.
        let mut logged = (u32::MAX, false);
        while let Some(s) = events.next().await {
            let step = ((s.as_frac() * 100.0).round() as u32, s.blocked().is_some());
            if step != logged {
                logged = step;
                crate::logln!("tor: {s}");
            }
            let now = status_of(&s);
            tell.send_if_modified(|old| {
                let changed = *old != now;
                *old = now;
                changed
            });
        }
    });
    let (c1, c2) = (client.clone(), client);
    let tor = Tor {
        connect: Box::new(move |host, port| {
            let c = c1.clone();
            Box::pin(async move {
                c.connect((host.as_str(), port))
                    .await
                    .map_err(io::Error::other)
            })
        }),
        status,
        proxy,
    };
    if TOR.set(tor).is_err() {
        return Err("o Tor já estava ligado".into());
    }
    tokio::spawn(async move {
        if let Err(e) = c2.bootstrap().await {
            crate::logln!("tor: não entrou na rede: {e}");
        } else {
            crate::logln!("tor: conectado");
        }
    });
    tokio::spawn(serve(listener, user, pass));
    crate::logln!("tor: ligado");
    Ok(())
}

fn status_of(s: &BootstrapStatus) -> Status {
    if s.ready_for_traffic() {
        Status::Ready
    } else if let Some(b) = s.blocked() {
        Status::Blocked(b.to_string())
    } else {
        Status::Joining(s.as_frac())
    }
}

/// Bytes from the operating system's generator, through ring.
fn random<const N: usize>() -> Result<[u8; N], String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| "sem a aleatoriedade do sistema".to_owned())?;
    Ok(out)
}

/// Compares in constant time.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether the app goes through Tor (chosen at the start).
pub fn active() -> bool {
    TOR.get().is_some()
}

/// The proxy the HTTP clients go through, when Tor is on.
pub fn proxy() -> Option<&'static str> {
    TOR.get().map(|t| t.proxy.as_str())
}

pub fn status() -> Option<Status> {
    TOR.get().map(|t| t.status.borrow().clone())
}

/// Told each time the status changes (the interface redraws then).
pub fn changes() -> Option<watch::Receiver<Status>> {
    TOR.get().map(|t| t.status.clone())
}

/// A connection to `host:port` through Tor (it waits for Tor to be on the
/// network first).
pub async fn connect(host: &str, port: u16) -> io::Result<DataStream> {
    let tor = TOR
        .get()
        .ok_or_else(|| io::Error::other("o Tor não está ligado"))?;
    (tor.connect)(host.to_owned(), port).await
}

// ---------------------------------------------------------------- the proxy

/// The SOCKS5 proxy (RFC 1928, with the user and password of RFC 1929)
/// the HTTP clients use: on 127.0.0.1 only, and only for whoever knows
/// the password, which never leaves the app.
async fn serve(listener: TcpListener, user: String, pass: String) {
    let (user, pass) = (Arc::new(user), Arc::new(pass));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (user, pass) = (user.clone(), pass.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &user, &pass).await {
                crate::logln!("tor: proxy: {e}");
            }
        });
    }
}

const VERSION: u8 = 5;
const USER_PASS: u8 = 2;
const NO_METHOD: u8 = 0xff;
const CONNECT: u8 = 1;
const REP_OK: u8 = 0;
const REP_FAILURE: u8 = 1;
const REP_HOST_UNREACHABLE: u8 = 4;
const REP_COMMAND: u8 = 7;
const REP_ADDRESS: u8 = 8;

async fn reply(s: &mut TcpStream, code: u8) -> io::Result<()> {
    s.write_all(&[VERSION, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

async fn handle(mut s: TcpStream, user: &str, pass: &str) -> io::Result<()> {
    s.set_nodelay(true)?;
    // The methods the client offers: only user and password will do.
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != VERSION {
        return Err(io::Error::other("não é SOCKS5"));
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&USER_PASS) {
        s.write_all(&[VERSION, NO_METHOD]).await?;
        return Err(io::Error::other("sem senha"));
    }
    s.write_all(&[VERSION, USER_PASS]).await?;
    let mut ver = [0u8; 1];
    s.read_exact(&mut ver).await?;
    let given_user = read_field(&mut s).await?;
    let given_pass = read_field(&mut s).await?;
    let ok = ver[0] == 1 && same(&given_user, user.as_bytes()) & same(&given_pass, pass.as_bytes());
    s.write_all(&[1, if ok { 0 } else { 1 }]).await?;
    if !ok {
        return Err(io::Error::other("senha errada"));
    }
    // What to connect to.
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[0] != VERSION || req[1] != CONNECT {
        reply(&mut s, REP_COMMAND).await?;
        return Err(io::Error::other("só CONNECT"));
    }
    let host = match req[3] {
        1 => {
            let mut ip = [0u8; 4];
            s.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => String::from_utf8(read_field(&mut s).await?)
            .map_err(|_| io::Error::other("nome inválido"))?,
        4 => {
            let mut ip = [0u8; 16];
            s.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => {
            reply(&mut s, REP_ADDRESS).await?;
            return Err(io::Error::other("endereço inválido"));
        }
    };
    let port = s.read_u16().await?;
    match connect(&host, port).await {
        Ok(mut tor) => {
            reply(&mut s, REP_OK).await?;
            // Either side closing ends it; what was on its way gets there.
            let _ = tokio::io::copy_bidirectional(&mut s, &mut tor).await;
            Ok(())
        }
        Err(e) => {
            let code = if e.kind() == io::ErrorKind::Other {
                REP_HOST_UNREACHABLE
            } else {
                REP_FAILURE
            };
            reply(&mut s, code).await?;
            Err(io::Error::other(format!("{host}:{port}: {e}")))
        }
    }
}

/// A length byte and that many bytes.
async fn read_field(s: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 1];
    s.read_exact(&mut len).await?;
    let mut out = vec![0u8; usize::from(len[0])];
    s.read_exact(&mut out).await?;
    Ok(out)
}
