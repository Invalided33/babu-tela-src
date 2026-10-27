//! TURN over TCP and TLS, for networks that let no UDP out.
//!
//! Company and school Wi-Fi, hotels and some mobile carriers only let TCP
//! through, often only to ports 80 and 443. The relay (TURN) speaks TCP and
//! TLS for exactly that, but this app's WebRTC stack only speaks TURN over
//! UDP. So for those addresses the app runs a small relay of its own on this
//! computer: the WebRTC stack talks plain UDP TURN to it, and every message
//! goes, unchanged, over a TCP or TLS connection to the real server.
//!
//! - The relay listens on the computer's own network address (the one the
//!   way to the server leaves from), not on 127.0.0.1: the WebRTC stack binds
//!   one socket per network card, and Windows will not send from those to
//!   the loopback address. It only takes datagrams from that same address,
//!   so nothing else on the network can use it.
//! - One connection per local socket: a TURN allocation belongs to the
//!   transport address it was made from (RFC 8656 §2.2), so two sockets
//!   must not share one.
//! - On the stream, messages delimit themselves: STUN by its length field,
//!   ChannelData by its own, padded to four bytes (RFC 8656 §12.5).
//! - Credentials and integrity pass through untouched: MESSAGE-INTEGRITY
//!   covers the message, not the path it took. What the server allocates is
//!   still a UDP relay, so the other side sees an ordinary relay candidate.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use webrtc::peer_connection::RTCIceServer;

use crate::model::IceConfig;

/// The longest closing the relays waits for what they still carry to reach
/// the server.
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// A TURN server reached over a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// Reads a TURN address the WebRTC stack cannot use by itself:
/// `turn:host:port?transport=tcp`, or `turns:` (TLS, always over TCP).
pub fn stream_target(url: &str) -> Option<Target> {
    let (scheme, rest) = url.split_once(':')?;
    let tls = match scheme.to_ascii_lowercase().as_str() {
        "turn" => false,
        "turns" => true,
        _ => return None,
    };
    let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
    let transport = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("transport="))
        .map(str::to_ascii_lowercase);
    match (tls, transport.as_deref()) {
        (false, Some("tcp")) | (true, None | Some("tcp")) => {}
        _ => return None,
    }
    let default_port = if tls { 5349 } else { 3478 };
    let (host, port) = if let Some(v6) = addr.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = after.strip_prefix(':').map(str::parse).transpose().ok()?;
        (host.to_owned(), port.unwrap_or(default_port))
    } else {
        match addr.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), port.parse().ok()?),
            None => (addr.to_owned(), default_port),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some(Target { host, port, tls })
}

/// Of everything the server offered over TCP, at most one TLS address (port
/// 443 first: it looks like any secure site and passes nearly every
/// firewall) and one plain TCP address (port 80 first). More would only
/// open more connections that do the same thing.
fn choose(ice: &IceConfig) -> Vec<(Target, String, String)> {
    let mut found: Vec<(Target, String, String)> = Vec::new();
    for server in &ice.ice_servers {
        let (Some(user), Some(pass)) = (&server.username, &server.credential) else {
            continue;
        };
        if user.is_empty() || pass.is_empty() {
            continue;
        }
        for url in &server.urls {
            if let Some(t) = stream_target(url) {
                found.push((t, user.clone(), pass.clone()));
            }
        }
    }
    let pick = |tls: bool, best: u16| {
        let mut of_kind: Vec<_> = found.iter().filter(|(t, ..)| t.tls == tls).collect();
        of_kind.sort_by_key(|(t, ..)| t.port != best);
        of_kind.first().map(|c| (*c).clone())
    };
    [pick(true, 443), pick(false, 80)]
        .into_iter()
        .flatten()
        .collect()
}

/// The local relays of one call. `close` ends them once what they carry
/// has gone out; dropping it cuts them off.
pub struct Relay {
    servers: Vec<RTCIceServer>,
    tasks: Vec<JoinHandle<()>>,
    /// Tells the fronts the call is over.
    over: watch::Sender<bool>,
}

impl Relay {
    pub async fn start(ice: Option<&IceConfig>) -> Relay {
        let mut relay = Relay {
            servers: Vec::new(),
            tasks: Vec::new(),
            over: watch::Sender::new(false),
        };
        let Some(ice) = ice else { return relay };
        for (target, username, credential) in choose(ice) {
            let Some(ip) = own_address(&target).await else {
                continue;
            };
            let Some((socket, direct)) = bind(ip) else {
                continue;
            };
            let Ok(local) = socket.local_addr() else {
                continue;
            };
            let over = relay.over.subscribe();
            relay
                .tasks
                .push(tokio::spawn(front(socket, direct, target, over)));
            relay.servers.push(RTCIceServer {
                urls: vec![format!("turn:{local}?transport=udp")],
                username,
                credential,
            });
        }
        relay
    }

    /// What to hand the WebRTC stack, next to the servers it reaches itself.
    pub fn servers(&self) -> Vec<RTCIceServer> {
        self.servers.clone()
    }

    /// Closes the relays once the call's connections have closed: what
    /// those sent on their way out (the release of each relay allocation)
    /// still reaches the server, and then every connection to it ends.
    pub async fn close(mut self) {
        self.over.send_replace(true);
        let tasks = std::mem::take(&mut self.tasks);
        let aborts: Vec<_> = tasks.iter().map(JoinHandle::abort_handle).collect();
        let done = futures_util::future::join_all(tasks);
        if tokio::time::timeout(CLOSE_WAIT, done).await.is_err() {
            for task in aborts {
                task.abort();
            }
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        // Each front owns its connections' senders, so they go with it.
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// A socket for a front on `ip`, and a second handle to the same socket
/// for reading it directly when the call ends (see `front`). Everything
/// the call sends through the relay passes it, video in bursts, so it gets
/// the same room as the call's own sockets (udp.rs).
fn bind(ip: IpAddr) -> Option<(UdpSocket, std::net::UdpSocket)> {
    let socket = std::net::UdpSocket::bind((ip, 0)).ok()?;
    super::udp::roomy(&socket);
    socket.set_nonblocking(true).ok()?;
    let direct = socket.try_clone().ok()?;
    direct.set_nonblocking(true).ok()?;
    Some((UdpSocket::from_std(socket).ok()?, direct))
}

/// The address this computer reaches `target` from: the network card the
/// route to it leaves through. Asking costs no packet (a UDP "connect" only
/// looks up the route), so it works where UDP is blocked too.
async fn own_address(target: &Target) -> Option<IpAddr> {
    // Through Tor, not even the relay's name is looked up here: the way out
    // to the internet will do.
    let server = if crate::tor::active() {
        SocketAddr::from(([1, 1, 1, 1], 443))
    } else {
        tokio::net::lookup_host((target.host.as_str(), target.port))
            .await
            .ok()?
            .find(SocketAddr::is_ipv4)?
    };
    let probe = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    probe.connect(server).await.ok()?;
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// One local socket's way to the server: its queue, and the task that
/// empties it.
struct Conn {
    tx: mpsc::Sender<Vec<u8>>,
    task: JoinHandle<()>,
}

/// The local end: takes datagrams from the WebRTC stack's sockets and hands
/// each one to the connection of the socket it came from, until the call is
/// `over`.
async fn front(
    socket: UdpSocket,
    direct: std::net::UdpSocket,
    target: Target,
    mut over: watch::Receiver<bool>,
) {
    let socket = Arc::new(socket);
    let Ok(own) = socket.local_addr().map(|a| a.ip()) else {
        return;
    };
    let mut conns: HashMap<SocketAddr, Conn> = HashMap::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let got = tokio::select! {
            got = socket.recv_from(&mut buf) => got,
            _ = over.wait_for(|over| *over) => break,
        };
        let Ok((n, from)) = got else {
            // A reset from a socket that went away (Windows reports those
            // on UDP too); the others carry on.
            continue;
        };
        // Only this computer's own sockets: the address is on the network,
        // and nobody else gets to open connections through it.
        if from.ip() != own {
            continue;
        }
        let Some(framed) = to_stream(&buf[..n]) else {
            continue;
        };
        conns.retain(|_, c| !c.tx.is_closed());
        let conn = conns.entry(from).or_insert_with(|| {
            let (tx, rx) = mpsc::channel(512);
            let task = tokio::spawn(connection(target.clone(), from, socket.clone(), rx));
            Conn { tx, task }
        });
        // A full queue means the server is not keeping up; media is better
        // dropped than late.
        let _ = conn.tx.try_send(framed);
    }
    // The call's connections are closed, and the last thing each sent (the
    // release of its allocation) is already on this socket, but tokio only
    // reads once the system has told it there is something. So the socket
    // is read directly until it is empty, and what came from a socket that
    // has a connection goes on.
    loop {
        match direct.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(conn) = conns.get(&from)
                    && let Some(framed) = to_stream(&buf[..n])
                {
                    let _ = conn.tx.try_send(framed);
                }
            }
            // A reset from a socket that already went away; what follows
            // still counts.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            // Empty.
            Err(_) => break,
        }
    }
    // Each connection sends what it holds, then closes.
    let tasks: Vec<JoinHandle<()>> = conns.into_values().map(|c| c.task).collect();
    futures_util::future::join_all(tasks).await;
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

async fn connect(target: &Target) -> io::Result<Box<dyn Stream>> {
    let timeout = Duration::from_secs(10);
    let tcp: Box<dyn Stream> = if crate::tor::active() {
        // Through Tor (tor.rs): the relay sees a Tor exit, not this PC.
        Box::new(
            tokio::time::timeout(
                Duration::from_secs(60),
                crate::tor::connect(&target.host, target.port),
            )
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??,
        )
    } else {
        let tcp = tokio::time::timeout(
            timeout,
            TcpStream::connect((target.host.as_str(), target.port)),
        )
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        tcp.set_nodelay(true)?;
        Box::new(tcp)
    };
    if !target.tls {
        return Ok(tcp);
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(io::Error::other)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(target.host.clone())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let tls = tokio::time::timeout(
        timeout,
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp),
    )
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    Ok(Box::new(tls))
}

/// One local socket's connection to the server. Ends when the call does
/// (the front drops its sender) or when the server hangs up; a new message
/// from the same socket opens a fresh one.
async fn connection(
    target: Target,
    client: SocketAddr,
    socket: Arc<UdpSocket>,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    let kind = if target.tls { "TLS" } else { "TCP" };
    let stream = match connect(&target).await {
        Ok(s) => s,
        Err(e) => {
            crate::logln!(
                "relé {kind} {}:{} para {client}: {e}",
                target.host,
                target.port
            );
            return;
        }
    };
    let trace = std::env::var_os("BABUTELA_RELAY_LOG").is_some_and(|v| v == "1");
    if trace {
        crate::logln!(
            "relé {kind} {}:{} aberto para {client}",
            target.host,
            target.port
        );
    }
    let (mut read, mut write) = tokio::io::split(stream);
    let to_server = async {
        while let Some(msg) = rx.recv().await {
            if trace {
                crate::logln!("relé → {:02x}{:02x} {} bytes", msg[0], msg[1], msg.len());
            }
            if write.write_all(&msg).await.is_err() {
                return;
            }
        }
        // The call is over: whatever is still buffered goes out, and the
        // server sees the connection end cleanly.
        let _ = write.shutdown().await;
    };
    let from_server = async {
        let mut head = [0u8; 4];
        let mut body = Vec::new();
        loop {
            if read.read_exact(&mut head).await.is_err() {
                break;
            }
            let Some((rest, meaningful)) = stream_len(head) else {
                break; // not TURN: nothing after this can be trusted
            };
            body.clear();
            body.extend_from_slice(&head);
            body.resize(4 + rest, 0);
            if read.read_exact(&mut body[4..]).await.is_err() {
                break;
            }
            if trace {
                crate::logln!("relé ← {:02x}{:02x} {meaningful} bytes", head[0], head[1]);
            }
            // Over UDP, ChannelData goes without its padding.
            let _ = socket.send_to(&body[..meaningful], client).await;
        }
    };
    tokio::select! {
        () = to_server => {}
        () = from_server => {}
    }
}

/// A datagram from the WebRTC stack as it must go on the stream, or `None`
/// for something that is not TURN.
fn to_stream(datagram: &[u8]) -> Option<Vec<u8>> {
    if datagram.len() < 4 {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([datagram[2], datagram[3]]));
    match datagram[0] >> 6 {
        // STUN: the length covers everything after the 20-byte header and
        // is always a multiple of four.
        0b00 if datagram.len() == 20 + len => Some(datagram.to_vec()),
        // ChannelData: 4 bytes of header, the data, then padding to four.
        0b01 if datagram.len() >= 4 + len => {
            let mut out = datagram[..4 + len].to_vec();
            out.resize((4 + len + 3) & !3, 0);
            Some(out)
        }
        _ => None,
    }
}

/// For a message that starts with `head` on the stream: how many more bytes
/// it takes there, and how many of all of them are the message itself (the
/// rest is ChannelData's padding).
fn stream_len(head: [u8; 4]) -> Option<(usize, usize)> {
    let len = usize::from(u16::from_be_bytes([head[2], head[3]]));
    match head[0] >> 6 {
        0b00 => Some((16 + len, 20 + len)),
        0b01 => {
            let total = 4 + len;
            Some((((total + 3) & !3) - 4, total))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IceServer;

    #[test]
    fn reads_the_addresses_that_need_a_stream() {
        let t = |host: &str, port, tls| {
            Some(Target {
                host: host.into(),
                port,
                tls,
            })
        };
        assert_eq!(
            stream_target("turn:turn.cloudflare.com:80?transport=tcp"),
            t("turn.cloudflare.com", 80, false)
        );
        assert_eq!(
            stream_target("turns:turn.cloudflare.com:443?transport=tcp"),
            t("turn.cloudflare.com", 443, true)
        );
        assert_eq!(
            stream_target("turns:relay.example"),
            t("relay.example", 5349, true)
        );
        assert_eq!(
            stream_target("turn:[2001:db8::1]:3478?transport=tcp"),
            t("2001:db8::1", 3478, false)
        );
        // What the stack reaches by itself, or is not TURN at all.
        assert_eq!(
            stream_target("turn:turn.cloudflare.com:3478?transport=udp"),
            None
        );
        assert_eq!(stream_target("turn:turn.cloudflare.com:3478"), None);
        assert_eq!(stream_target("stun:stun.cloudflare.com:3478"), None);
        assert_eq!(stream_target("turns:x:443?transport=udp"), None);
        assert_eq!(stream_target("turn::80?transport=tcp"), None);
    }

    #[test]
    fn picks_one_tls_and_one_tcp_address() {
        let ice = IceConfig {
            ice_servers: vec![
                IceServer {
                    urls: vec!["stun:stun.cloudflare.com:3478".into()],
                    ..Default::default()
                },
                IceServer {
                    urls: [
                        "turn:turn.cloudflare.com:3478?transport=udp",
                        "turn:turn.cloudflare.com:3478?transport=tcp",
                        "turns:turn.cloudflare.com:5349?transport=tcp",
                        "turn:turn.cloudflare.com:80?transport=tcp",
                        "turns:turn.cloudflare.com:443?transport=tcp",
                    ]
                    .map(String::from)
                    .to_vec(),
                    username: Some("u".into()),
                    credential: Some("p".into()),
                },
            ],
            relay_only: false,
        };
        let chosen: Vec<(u16, bool)> = choose(&ice).iter().map(|(t, ..)| (t.port, t.tls)).collect();
        assert_eq!(chosen, [(443, true), (80, false)]);
    }

    #[test]
    fn frames_messages_for_the_stream() {
        // A STUN binding request with no attributes.
        let mut stun = vec![0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42];
        stun.extend_from_slice(&[7; 12]);
        assert_eq!(to_stream(&stun), Some(stun.clone()));
        assert_eq!(stream_len([0x00, 0x01, 0x00, 0x00]), Some((16, 20)));
        assert_eq!(to_stream(&stun[..19]), None);

        // ChannelData with 5 bytes of data: padded to 12 on the stream.
        let chan = [0x40, 0x00, 0x00, 0x05, 1, 2, 3, 4, 5];
        let framed = to_stream(&chan).unwrap();
        assert_eq!(framed, [0x40, 0x00, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0]);
        // Read back: 8 more bytes after the head, 9 of the 12 are the message.
        assert_eq!(stream_len([0x40, 0x00, 0x00, 0x05]), Some((8, 9)));
        // Padding a sender already added does not count twice.
        let padded = [0x40, 0x00, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0];
        assert_eq!(to_stream(&padded).unwrap().len(), 12);
        // Neither STUN nor ChannelData.
        assert_eq!(to_stream(&[0x80, 0, 0, 0]), None);
        assert_eq!(stream_len([0xc0, 0, 0, 0]), None);
    }

    /// The relay end to end against a fake TURN server on a local TCP port:
    /// what the WebRTC stack sends over UDP arrives framed, and the answers
    /// come back as datagrams, padding gone.
    #[tokio::test]
    async fn carries_both_ways_over_tcp() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let echo = tokio::spawn(async move {
            let (mut s, _) = server.accept().await.unwrap();
            let mut got = vec![0u8; 12];
            s.read_exact(&mut got).await.unwrap();
            // Sends it back, followed by a STUN message.
            let mut reply = got.clone();
            let mut stun = vec![0x01, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42];
            stun.extend_from_slice(&[9; 12]);
            reply.extend_from_slice(&stun);
            s.write_all(&reply).await.unwrap();
            (got, s)
        });
        let (socket, direct) = bind(IpAddr::from([127, 0, 0, 1])).unwrap();
        let front_addr = socket.local_addr().unwrap();
        let over = watch::Sender::new(false);
        let front_task = tokio::spawn(front(
            socket,
            direct,
            Target {
                host: "127.0.0.1".into(),
                port,
                tls: false,
            },
            over.subscribe(),
        ));
        let me = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        me.send_to(&[0x40, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5], front_addr)
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let wait = Duration::from_secs(5);
        let (n, from) = tokio::time::timeout(wait, me.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, front_addr);
        assert_eq!(&buf[..n], &[0x40, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5]);
        let (n, _) = tokio::time::timeout(wait, me.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 20);
        assert_eq!(buf[0], 0x01);
        let (got, _keep) = echo.await.unwrap();
        assert_eq!(got, [0x40, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5, 0, 0, 0]);
        front_task.abort();
    }
}
