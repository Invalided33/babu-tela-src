//! A network to test on (BABUTELA_REDE_TESTE): what a line between two homes
//! does to the datagrams this app sends, on every WebRTC socket it has
//! (calls and files sent directly; udp.rs wraps them). Two copies of the app
//! on one PC talk over the loopback, where nothing is ever late or lost; with
//! this, the same test sees a round trip, losses and a slow upload, and
//! whatever depends on them (SCTP's congestion control, above all) shows.
//!
//!   BABUTELA_REDE_TESTE=atraso=20,perda=0.1,banda=100,fila=100
//!
//! - `atraso`: milliseconds each datagram takes to get there, one way (two
//!   apps with 20 each make a round trip of 40 ms).
//! - `perda`: percent of the datagrams lost on the way, at random.
//! - `banda`: this app's upload, in Mbit/s. What goes faster waits in a
//!   queue in front of it, as in a home router. All the app's sockets share
//!   that one queue, as a PC's connections share its router's: a call's
//!   voice waits behind a file sent directly at the same time.
//! - `fila`: how long that queue gets, in milliseconds of upload (100 when
//!   not given). What does not fit is dropped, as a full router drops it.
//!
//! Without the variable none of this exists: the sockets are tokio's own.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use webrtc::runtime::{AsyncUdpSocket, RecvMeta, Runtime as _, TokioRuntime, Transmit};

const ENV: &str = "BABUTELA_REDE_TESTE";

/// How the line behaves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Netem {
    delay: Duration,
    /// 0 to 1.
    loss: f64,
    /// Bytes a second, when the upload has a limit.
    rate: Option<f64>,
    queue: Duration,
}

impl Netem {
    /// The line the variable asks for, once; `None` without it. A variable
    /// that does not hold together stops the app: a test on the wrong
    /// network would measure nothing.
    pub(crate) fn from_env() -> Option<Netem> {
        static ONCE: OnceLock<Option<Netem>> = OnceLock::new();
        *ONCE.get_or_init(|| {
            let spec = std::env::var(ENV).ok().filter(|s| !s.trim().is_empty())?;
            match Netem::parse(&spec) {
                Ok(n) => {
                    crate::logln!("rede de teste: {spec}");
                    Some(n)
                }
                Err(e) => panic!("{ENV}={spec}: {e}"),
            }
        })
    }

    fn parse(spec: &str) -> Result<Netem, String> {
        let mut n = Netem {
            delay: Duration::ZERO,
            loss: 0.0,
            rate: None,
            queue: Duration::from_millis(100),
        };
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| format!("\"{part}\" não é chave=valor"))?;
            let number: f64 = value
                .trim()
                .parse()
                .ok()
                .filter(|v: &f64| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| format!("\"{value}\" não é um número"))?;
            match key.trim() {
                "atraso" => n.delay = Duration::from_secs_f64(number / 1000.0),
                "perda" if number <= 100.0 => n.loss = number / 100.0,
                "banda" if number > 0.0 => n.rate = Some(number * 1e6 / 8.0),
                "fila" => n.queue = Duration::from_secs_f64(number / 1000.0),
                other => return Err(format!("\"{other}={value}\" não serve")),
            }
        }
        Ok(n)
    }
}

/// `socket`, as tokio wraps it, behind the line: what it sends leaves after
/// the queue and the delay, from a thread that holds it until then; what it
/// receives comes as it came (the other app's own line delayed it).
pub(crate) fn wrap(
    socket: std::net::UdpSocket,
    netem: Netem,
) -> io::Result<Arc<dyn AsyncUdpSocket>> {
    let out = socket.try_clone()?;
    let inner = TokioRuntime.wrap_udp_socket(socket)?;
    let (tx, rx) = std::sync::mpsc::channel::<Datagram>();
    std::thread::Builder::new()
        .name("rede-de-teste".into())
        .spawn(move || {
            // In order: the departures only grow, and the delay is the same
            // for all. The sleep is Windows' high-resolution timer (std's).
            while let Ok(d) = rx.recv() {
                let wait = d.at.saturating_duration_since(Instant::now());
                if wait > Duration::from_micros(500) {
                    std::thread::sleep(wait);
                }
                // A full send buffer drops it, as a router would.
                let _ = out.send_to(&d.data, d.to);
            }
        })?;
    // Each socket its own dice.
    let port = inner.local_addr().map_or(0, |a| a.port());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    Ok(Arc::new(Impaired {
        inner,
        netem,
        line: Mutex::new(Line {
            rng: (nanos ^ (u64::from(port) << 40)) | 1,
        }),
        tx,
    }))
}

struct Datagram {
    at: Instant,
    to: SocketAddr,
    data: Vec<u8>,
}

/// The app's upload, one for all its sockets: when it is free for the next
/// datagram (`None` until the first).
static UPLOAD: Mutex<Option<Instant>> = Mutex::new(None);

/// When a datagram of `len` bytes handed over at `now` has gone out of an
/// upload of `rate` bytes a second that is free at `free_at` (which moves
/// past it); `None` when the queue in front already holds more than
/// `queue`, and a full router drops it.
fn admit(
    free_at: &mut Option<Instant>,
    now: Instant,
    len: usize,
    rate: f64,
    queue: Duration,
) -> Option<Instant> {
    let start = free_at.map_or(now, |f| f.max(now));
    if start - now > queue {
        return None;
    }
    let leaves = start + Duration::from_secs_f64(len as f64 / rate);
    *free_at = Some(leaves);
    Some(leaves)
}

/// A socket's dice.
struct Line {
    rng: u64,
}

impl Line {
    /// 0 to 1, xorshift.
    fn roll(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Impaired {
    inner: Arc<dyn AsyncUdpSocket>,
    netem: Netem,
    line: Mutex<Line>,
    tx: std::sync::mpsc::Sender<Datagram>,
}

impl fmt::Debug for Impaired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Impaired")
            .field("inner", &self.inner)
            .field("netem", &self.netem)
            .finish()
    }
}

impl AsyncUdpSocket for Impaired {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// One datagram per send: the line counts them one by one.
    fn max_gso_segments(&self) -> usize {
        1
    }

    fn max_gro_segments(&self) -> usize {
        self.inner.max_gro_segments()
    }

    fn poll_send(&self, _cx: &mut Context<'_>, t: &Transmit<'_>) -> Poll<io::Result<usize>> {
        let len = t.contents.len();
        let now = Instant::now();
        // Lost on the way, or dropped by a full queue: the sender never
        // knows, as with a real line.
        if self.netem.loss > 0.0 {
            let mut line = self.line.lock().unwrap_or_else(PoisonError::into_inner);
            if line.roll() < self.netem.loss {
                return Poll::Ready(Ok(len));
            }
        }
        let leaves = match self.netem.rate {
            Some(rate) => {
                let mut free_at = UPLOAD.lock().unwrap_or_else(PoisonError::into_inner);
                match admit(&mut free_at, now, len, rate, self.netem.queue) {
                    Some(leaves) => leaves,
                    None => return Poll::Ready(Ok(len)),
                }
            }
            None => now,
        };
        let _ = self.tx.send(Datagram {
            at: leaves + self.netem.delay,
            to: t.destination,
            data: t.contents.to_vec(),
        });
        Poll::Ready(Ok(len))
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_line_asked_for() {
        let n = Netem::parse("atraso=20, perda=0.5,banda=100,fila=50").unwrap();
        assert_eq!(n.delay, Duration::from_millis(20));
        assert!((n.loss - 0.005).abs() < 1e-12);
        assert_eq!(n.rate, Some(12_500_000.0));
        assert_eq!(n.queue, Duration::from_millis(50));
        let plain = Netem::parse("atraso=5").unwrap();
        assert_eq!(plain.rate, None);
        assert_eq!(plain.loss, 0.0);
        assert_eq!(plain.queue, Duration::from_millis(100));
        for bad in [
            "atraso",
            "atraso=x",
            "perda=101",
            "banda=0",
            "cor=3",
            "atraso=-1",
        ] {
            assert!(Netem::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_upload_is_one_queue_that_drops_what_does_not_fit() {
        // 1 Mbit/s: 1250 bytes take 10 ms, and a 25 ms queue holds two
        // behind the one going out.
        let (rate, queue) = (125_000.0, Duration::from_millis(25));
        let now = Instant::now();
        let ms = |at: Option<Instant>| at.map(|at| (at - now).as_secs_f64() * 1000.0);
        let mut free = None;
        let gone: Vec<Option<f64>> = (0..4)
            .map(|_| ms(admit(&mut free, now, 1250, rate, queue)))
            .collect();
        for (got, want) in gone.iter().zip([10.0, 20.0, 30.0]) {
            assert!((got.unwrap() - want).abs() < 0.01, "{gone:?}");
        }
        assert_eq!(gone[3], None, "the fourth does not fit");
        // Once the queue has gone out, it goes at once again.
        let later = now + Duration::from_millis(100);
        let at = admit(&mut free, later, 1250, rate, queue).unwrap();
        assert!(((at - later).as_secs_f64() * 1000.0 - 10.0).abs() < 0.01);
    }

    #[test]
    fn the_dice_are_fair_enough() {
        let mut line = Line { rng: 12345 };
        let lost = (0..100_000).filter(|_| line.roll() < 0.01).count();
        assert!((800..1200).contains(&lost), "{lost}");
    }
}
