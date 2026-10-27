//! The UDP sockets every WebRTC connection of the app runs on (calls and
//! files sent directly), and the relay's local ends (relay.rs), with kernel
//! buffers sized for bursts.
//!
//! Windows gives a UDP socket 64 KB to hold what arrived and was not read
//! yet: about fifty datagrams. A picture of a shared screen, or a fast file
//! transfer, comes in bursts larger than that, and what does not fit is
//! dropped before the WebRTC stack ever sees it: a lost video packet costs a
//! retransmission or a whole new picture, a lost SCTP packet a
//! retransmission and a smaller window (and, when it was the tail of a
//! burst, a timeout of a second). Browsers raise it for the same reason
//! (Chrome's video sockets take 256 KB), and so do QUIC stacks (quic-go asks
//! for about 7 MB); between two copies of this app on one PC, 400 MiB sent
//! directly went at 72 to 76 MB/s with 4 MiB, against 21 to 41 MB/s (and
//! once a stall) with Windows' default. The size is a ceiling, not an
//! allocation: the kernel only holds what has actually arrived.
//!
//! webrtc binds its sockets itself and hands each to its runtime to wrap
//! ("bound and configured before being wrapped", says `Runtime`): this is
//! tokio's runtime, as before, with the buffers set as each socket comes.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use webrtc::runtime::{
    AsyncInterval, AsyncTcpListener, AsyncTcpStream, AsyncUdpSocket, JoinHandle, TokioRuntime,
    UdpSockRef, UdpSocketState,
};

/// What each socket asks for, to receive and to send.
pub(crate) const BUFFER: usize = 4 << 20;

/// Sets `socket`'s buffers to BUFFER. A socket Windows gives less to keeps
/// working with what it got; the log says so, once.
pub(crate) fn roomy(socket: &std::net::UdpSocket) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let state = match UdpSocketState::new(UdpSockRef::from(socket)) {
        Ok(state) => state,
        Err(e) => {
            crate::logln!("udp: não deu para ajustar o socket: {e}");
            return;
        }
    };
    let set = |recv: bool| {
        let sock = UdpSockRef::from(socket);
        let asked = if recv {
            state.set_recv_buffer_size(sock, BUFFER)
        } else {
            state.set_send_buffer_size(sock, BUFFER)
        };
        let got = if recv {
            state.recv_buffer_size(UdpSockRef::from(socket))
        } else {
            state.send_buffer_size(UdpSockRef::from(socket))
        };
        match (asked, got) {
            (Ok(()), Ok(got)) if got >= BUFFER => None,
            (asked, got) => Some(format!(
                "{}: pedi {} KB, ficou {} ({})",
                if recv { "recepção" } else { "envio" },
                BUFFER >> 10,
                got.map_or_else(|e| e.to_string(), |n| format!("{} KB", n >> 10)),
                asked
                    .err()
                    .map_or_else(|| "aceito".to_owned(), |e| e.to_string()),
            )),
        }
    };
    let short: Vec<String> = [set(true), set(false)].into_iter().flatten().collect();
    if !short.is_empty() && !WARNED.swap(true, Ordering::Relaxed) {
        crate::logln!("udp: buffer menor que o pedido: {}", short.join("; "));
    }
}

/// Tokio, with roomy UDP sockets (see the module's comment).
#[derive(Debug)]
pub(crate) struct Runtime;

impl webrtc::runtime::Runtime for Runtime {
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) -> Box<dyn JoinHandle> {
        TokioRuntime.spawn(future)
    }

    fn spawn_reactor(
        &self,
        size: usize,
        future: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) -> Box<dyn JoinHandle> {
        TokioRuntime.spawn_reactor(size, future)
    }

    fn wrap_udp_socket(
        &self,
        socket: std::net::UdpSocket,
    ) -> std::io::Result<Arc<dyn AsyncUdpSocket>> {
        roomy(&socket);
        TokioRuntime.wrap_udp_socket(socket)
    }

    fn wrap_tcp_listener(
        &self,
        listener: std::net::TcpListener,
    ) -> std::io::Result<Arc<dyn AsyncTcpListener>> {
        TokioRuntime.wrap_tcp_listener(listener)
    }

    fn connect_tcp<'a>(
        &'a self,
        remote_addr: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Arc<dyn AsyncTcpStream>>> + Send + 'a>> {
        TokioRuntime.connect_tcp(remote_addr)
    }

    fn resolve_host<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'a>> {
        Box::pin(async move { TokioRuntime.resolve_host(host).await })
    }

    fn now(&self) -> Instant {
        TokioRuntime.now()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        TokioRuntime.sleep(duration)
    }

    fn interval(&self, period: Duration) -> Box<dyn AsyncInterval> {
        TokioRuntime.interval(period)
    }

    fn block_on(&self, future: Pin<Box<dyn Future<Output = ()> + '_>>) {
        TokioRuntime.block_on(future);
    }

    fn yield_now(&self) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        TokioRuntime.yield_now()
    }

    fn name(&self) -> &'static str {
        "tokio"
    }
}
