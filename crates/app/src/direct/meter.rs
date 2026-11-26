//! What a transfer's connection does while it goes, second by second: how
//! fast it goes, where SCTP's congestion control stands (its window, the
//! round trip, what it measured of the path, the losses it found and how it
//! recovered), which way the data takes (straight or through the relay, over
//! UDP or TCP), and what the app waited on, the connection or the disk.
//!
//! The meter runs beside the transfer, on a task of its own: a transfer
//! held up (a send waiting for room, a stall) is measured all the same.
//! Every transfer leaves a line in the log when it ends, and the tests'
//! report carries the seconds (`direct`), which `cargo xtask direto` prints:
//! a slow transfer says why it is slow. The numbers come from the
//! connection's getStats, whose `sctp-transport` entry vendor/rtc adds
//! (vendor/rtc-sctp, `Association::congestion`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rtc::peer_connection::transport::RTCIceCandidateType;
use rtc::statistics::StatsSelector;
use rtc::statistics::report::{RTCStatsReport, RTCStatsReportEntry};
use rtc::statistics::stats::sctp_transport::RTCSctpTransportStats;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use webrtc::peer_connection::PeerConnection;

/// How often a transfer is sampled.
const EVERY: Duration = Duration::from_secs(1);

/// The seconds a report keeps (the last ones; a longer transfer keeps its
/// totals all the same).
const KEEP: usize = 1800;

/// A transfer's readings, shared with the report.
pub(super) type Readings = Arc<Mutex<Value>>;

pub(super) fn readings() -> Readings {
    Arc::new(Mutex::new(Value::Null))
}

/// What the transfer tells its meter as it goes.
pub(super) struct Gauge {
    /// Bytes of the file moved (handed to the connection, or on the disk).
    bytes: Arc<AtomicU64>,
    /// Waited on the connection (a send held until there was room) and on
    /// the disk, nanoseconds, since the last sample.
    net: AtomicU64,
    disk: AtomicU64,
    /// When the file started to flow, and the bytes then: the meter's clock
    /// starts there, not at the setting up of the connection or the wait for
    /// a turn.
    flowing: Mutex<Option<(Instant, u64)>>,
    /// When the last of it arrived (the downloader says, chunk by chunk).
    last: Mutex<Option<Instant>>,
}

impl Gauge {
    /// A chunk of the file arrived just now (the downloader's): the
    /// connection's own time ends with the last, before the disk's flush.
    pub(super) fn arrived(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    pub(super) fn waited_on_net(&self, d: Duration) {
        self.net.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub(super) fn waited_on_disk(&self, d: Duration) {
        self.disk.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub(super) fn flowing(&self) {
        let mut at = self.flowing.lock().unwrap_or_else(PoisonError::into_inner);
        at.get_or_insert((Instant::now(), self.bytes.load(Ordering::Relaxed)));
    }

    fn take(counter: &AtomicU64) -> Duration {
        Duration::from_nanos(counter.swap(0, Ordering::Relaxed))
    }
}

/// A running meter: `finish` ends it, its last line in the log.
pub(super) struct Meter {
    pub gauge: Arc<Gauge>,
    stop: Option<oneshot::Sender<String>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Meter {
    /// Measures the transfer on `pc` (`role`: "envio" or "download"), whose
    /// bytes so far `bytes` counts; its totals go to `file` too when it
    /// ends.
    pub(super) fn start(
        role: &'static str,
        readings: Readings,
        pc: Arc<dyn PeerConnection>,
        bytes: Arc<AtomicU64>,
        file: Option<PathBuf>,
    ) -> Meter {
        let gauge = Arc::new(Gauge {
            bytes,
            net: AtomicU64::new(0),
            disk: AtomicU64::new(0),
            flowing: Mutex::new(None),
            last: Mutex::new(None),
        });
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(role, readings, pc, gauge.clone(), file, stopped));
        Meter {
            gauge,
            stop: Some(stop),
            task: Some(task),
        }
    }

    /// The transfer of `what` ended: its totals go in the log and the
    /// report.
    pub(super) async fn finish(mut self, what: String) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(what);
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn run(
    role: &'static str,
    readings: Readings,
    pc: Arc<dyn PeerConnection>,
    gauge: Arc<Gauge>,
    file: Option<PathBuf>,
    mut stopped: oneshot::Receiver<String>,
) {
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut seconds: Option<Seconds> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let Some(s) = seconds.as_mut() else {
                    let at = *gauge.flowing.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some((at, bytes)) = at {
                        seconds = Some(Seconds::new(role, readings.clone(), at, bytes));
                    }
                    continue;
                };
                let stats = pc.get_stats(Instant::now(), StatsSelector::None).await;
                s.sample(
                    gauge.bytes.load(Ordering::Relaxed),
                    Gauge::take(&gauge.net),
                    Gauge::take(&gauge.disk),
                    &stats,
                );
            }
            what = &mut stopped => {
                if let Some(mut s) = seconds {
                    s.add_waits(Gauge::take(&gauge.net), Gauge::take(&gauge.disk));
                    s.arrived = *gauge.last.lock().unwrap_or_else(PoisonError::into_inner);
                    s.finish(
                        gauge.bytes.load(Ordering::Relaxed),
                        &what.unwrap_or_default(),
                        file.as_deref(),
                    );
                }
                return;
            }
        }
    }
}

/// The counters that only grow, as one sample saw them.
#[derive(Clone, Copy, Default)]
struct Counts {
    fast_recoveries: u64,
    fast_retrans: u64,
    timeouts: u64,
    resent: u64,
    lost: u64,
    dup: u64,
    sacks_in: u64,
    sacks_out: u64,
}

impl Counts {
    fn of(s: &RTCSctpTransportStats) -> Counts {
        Counts {
            fast_recoveries: s.fast_recoveries,
            fast_retrans: s.fast_retransmissions,
            timeouts: s.retransmission_timeouts,
            resent: s.timeout_retransmissions,
            lost: s.lost_data_chunks,
            dup: s.duplicate_data_chunks_received,
            sacks_in: s.sacks_received,
            sacks_out: s.sacks_sent,
        }
    }
}

/// The seconds of one transfer, and its totals.
struct Seconds {
    role: &'static str,
    readings: Readings,
    started: Instant,
    from_bytes: u64,
    last_at: Instant,
    last_bytes: u64,
    first: Option<Counts>,
    last: Counts,
    net: Duration,
    disk: Duration,
    path: Option<String>,
    /// The congestion control, as the last sample saw it.
    control: Option<String>,
    rtts: Vec<f64>,
    cwnds: Vec<f64>,
    seconds: Vec<Value>,
    /// Downloading, at the end: when the last of the file arrived.
    arrived: Option<Instant>,
}

impl Seconds {
    fn new(role: &'static str, readings: Readings, at: Instant, bytes: u64) -> Seconds {
        Seconds {
            role,
            readings,
            started: at,
            from_bytes: bytes,
            last_at: at,
            last_bytes: bytes,
            first: None,
            last: Counts::default(),
            net: Duration::ZERO,
            disk: Duration::ZERO,
            path: None,
            control: None,
            rtts: Vec::new(),
            cwnds: Vec::new(),
            seconds: Vec::new(),
            arrived: None,
        }
    }

    fn add_waits(&mut self, net: Duration, disk: Duration) {
        self.net += net;
        self.disk += disk;
    }

    /// A sample: `bytes` moved so far, what was waited on since the last
    /// one, and the connection's stats.
    fn sample(&mut self, bytes: u64, net: Duration, disk: Duration, stats: &RTCStatsReport) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_at).as_secs_f64().max(1e-3);
        let rate = bytes.saturating_sub(self.last_bytes) as f64 / dt;
        self.last_at = now;
        self.last_bytes = bytes;
        self.add_waits(net, disk);
        let (path, ice_rtt) = path(stats);
        if path.is_some() {
            self.path = path;
        }
        let pct = |d: Duration| round(100.0 * d.as_secs_f64() / dt, 0).min(100.0);
        let mut second = json!({
            "t": round(now.duration_since(self.started).as_secs_f64(), 1),
            "mb": round(bytes as f64 / 1e6, 1),
            "mbs": round(rate / 1e6, 2),
            "rede_pct": pct(net),
            "disco_pct": pct(disk),
            "rtt_ice_ms": ice_rtt.map(|r| round(r * 1000.0, 1)),
        });
        if let Some(s) = stats.sctp_transport() {
            let now_counts = Counts::of(s);
            if self.first.is_none() {
                self.first = Some(now_counts);
                self.last = now_counts;
            }
            let before = self.last;
            self.last = now_counts;
            let d = |f: fn(&Counts) -> u64| f(&now_counts).saturating_sub(f(&before));
            let srtt = s.smoothed_round_trip_time * 1000.0;
            if srtt > 0.0 {
                self.rtts.push(srtt);
            }
            self.cwnds.push(f64::from(s.congestion_window));
            self.control = Some(s.congestion_control.clone());
            if let Value::Object(m) = &mut second {
                for (k, v) in [
                    ("controle", json!(s.congestion_control)),
                    ("janela_kb", json!(s.congestion_window / 1024)),
                    ("limiar_kb", json!(s.slow_start_threshold / 1024)),
                    ("janela_dele_kb", json!(s.receiver_window / 1024)),
                    ("no_ar_kb", json!(s.unack_data / 1024)),
                    ("na_fila_kb", json!(s.pending_data / 1024)),
                    ("srtt_ms", json!(srtt.round())),
                    ("rto_ms", json!((s.retransmission_timeout * 1000.0).round())),
                    ("banda_mbs", json!(round(s.delivery_rate / 1e6, 2))),
                    ("ritmo_mbs", json!(round(s.pacing_rate / 1e6, 2))),
                    (
                        "rtt_min_ms",
                        json!(round(s.min_round_trip_time * 1000.0, 1)),
                    ),
                    ("recuperando", json!(s.in_fast_recovery)),
                    ("recuperacoes", json!(d(|c| c.fast_recoveries))),
                    ("reenvios_rapidos", json!(d(|c| c.fast_retrans))),
                    ("timeouts", json!(d(|c| c.timeouts))),
                    ("reenvios", json!(d(|c| c.resent))),
                    ("perdidos", json!(d(|c| c.lost))),
                    ("duplicados", json!(d(|c| c.dup))),
                    ("sacks_recebidos", json!(d(|c| c.sacks_in))),
                    ("sacks_enviados", json!(d(|c| c.sacks_out))),
                ] {
                    m.insert(k.into(), v);
                }
            }
        }
        if verbose() {
            crate::logln!("direto: {} {second}", self.role);
        }
        if self.seconds.len() >= KEEP {
            self.seconds.remove(0);
        }
        self.seconds.push(second);
        self.publish(bytes, false);
    }

    /// The totals so far (`done`: the transfer ended), into the readings.
    fn publish(&self, bytes: u64, done: bool) -> Value {
        let secs = self.last_at.duration_since(self.started).as_secs_f64();
        let moved = bytes.saturating_sub(self.from_bytes);
        let first = self.first.unwrap_or_default();
        let n = |f: fn(&Counts) -> u64| f(&self.last).saturating_sub(f(&first));
        let share = |d: Duration| {
            if secs > 0.0 {
                round(100.0 * d.as_secs_f64() / secs, 0).min(100.0)
            } else {
                0.0
            }
        };
        // From the first byte to the last that arrived (downloading): the
        // connection's own speed, without the disk's flush at the end.
        let data_secs = self
            .arrived
            .map(|at| at.duration_since(self.started).as_secs_f64())
            .filter(|s| *s > 0.0);
        let summary = json!({
            "papel": self.role,
            "acabou": done,
            "segundos": round(secs, 1),
            "mb": round(moved as f64 / 1e6, 1),
            "mbs": round(if secs > 0.0 { moved as f64 / secs / 1e6 } else { 0.0 }, 2),
            "dados_s": data_secs.map(|s| round(s, 2)),
            "mbs_dados": data_secs.map(|s| round(moved as f64 / s / 1e6, 2)),
            "caminho": self.path,
            "controle": self.control,
            "srtt_ms": median(&self.rtts).map(|r| r.round()),
            // The round trip's worst seconds: the queue a transfer builds
            // shows here (and every other connection on that line waits in
            // it too).
            "srtt_p95_ms": quantile(&self.rtts, 0.95).map(|r| r.round()),
            "janela_media_kb": mean(&self.cwnds).map(|c| (c / 1024.0).round()),
            "recuperacoes": n(|c| c.fast_recoveries),
            "reenvios_rapidos": n(|c| c.fast_retrans),
            "timeouts": n(|c| c.timeouts),
            "reenvios": n(|c| c.resent),
            "perdidos": n(|c| c.lost),
            "duplicados": n(|c| c.dup),
            "esperou_rede_pct": share(self.net),
            "esperou_disco_pct": share(self.disk),
        });
        *self.readings.lock().unwrap_or_else(PoisonError::into_inner) = json!({
            "resumo": summary.clone(),
            "segundos": self.seconds,
        });
        summary
    }

    /// The transfer ended with `bytes` moved: its line in the log, and in
    /// `file` (one JSON line per transfer, for whoever has to see later why
    /// one went slow).
    fn finish(&mut self, bytes: u64, what: &str, file: Option<&Path>) {
        self.last_at = Instant::now();
        let s = self.publish(bytes, true);
        crate::logln!("direto: {} de {what}: {s}", self.role);
        if let Some(file) = file {
            let mut line = s;
            line["quando"] = json!(chrono::Local::now().to_rfc3339());
            line["arquivo"] = json!(what);
            keep_line(file, &line.to_string());
        }
    }
}

/// The most a measurements file holds before it starts over (the one
/// before kept beside it as `.1`).
const FILE_MAX: u64 = 512 * 1024;

/// Appends `line` to `file`, keeping it small.
fn keep_line(file: &Path, line: &str) {
    use std::io::Write;
    if std::fs::metadata(file).is_ok_and(|m| m.len() > FILE_MAX) {
        let _ = std::fs::rename(file, file.with_extension("log.1"));
    }
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file);
    if let Ok(mut f) = opened {
        let _ = writeln!(f, "{line}");
    }
}

/// Whether every second goes to the log too (BABUTELA_DIRETO_LOG=1).
fn verbose() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("BABUTELA_DIRETO_LOG").is_some_and(|v| v == "1"))
}

/// The way the data takes, as "relay/udp (its address) → host/udp" (my
/// candidate, then theirs), and the round trip ICE last measured on it, in
/// seconds.
fn path(report: &RTCStatsReport) -> (Option<String>, Option<f64>) {
    let mut selected = None;
    for entry in report.iter() {
        if let RTCStatsReportEntry::Transport(t) = entry
            && !t.selected_candidate_pair_id.is_empty()
        {
            selected = Some(t.selected_candidate_pair_id.clone());
        }
    }
    let Some(pair) = selected.and_then(|id| {
        report.iter().find_map(|e| match e {
            RTCStatsReportEntry::IceCandidatePair(p) if p.stats.id == id => Some(p),
            _ => None,
        })
    }) else {
        return (None, None);
    };
    let bare = |id: &str| {
        id.trim_start_matches("RTCLocalIceCandidate_")
            .trim_start_matches("RTCRemoteIceCandidate_")
            .to_owned()
    };
    let describe = |want: &str, local: bool| {
        report.iter().find_map(|e| {
            let c = match e {
                RTCStatsReportEntry::LocalCandidate(c) if local => c,
                RTCStatsReportEntry::RemoteCandidate(c) if !local => c,
                _ => return None,
            };
            (bare(&c.stats.id) == bare(want)).then(|| {
                let kind = c.candidate_type.to_string();
                if c.candidate_type == RTCIceCandidateType::Relay && !c.url.is_empty() {
                    // The relay over TCP (voice/relay.rs) looks like one
                    // over UDP to the stack: its address, a local one, tells.
                    format!("{kind}/{} ({})", c.protocol, c.url)
                } else {
                    format!("{kind}/{}", c.protocol)
                }
            })
        })
    };
    let mine = describe(&pair.local_candidate_id, true);
    let theirs = describe(&pair.remote_candidate_id, false);
    let rtt = (pair.current_round_trip_time > 0.0).then_some(pair.current_round_trip_time);
    let way = match (mine, theirs) {
        (Some(a), Some(b)) => Some(format!("{a} → {b}")),
        (Some(a), None) => Some(a),
        _ => None,
    };
    (way, rtt)
}

fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}

fn median(xs: &[f64]) -> Option<f64> {
    quantile(xs, 0.5)
}

/// The value `q` (0 to 1) of the way up `xs` sorted.
fn quantile(xs: &[f64], q: f64) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let i = ((v.len() as f64 * q) as usize).min(v.len() - 1);
    Some(v[i])
}
