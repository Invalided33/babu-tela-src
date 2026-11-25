//! BBR congestion control for the sending side, and its pacer (Babu Tela).
//!
//! Reno (RFC 9260 §7.2) halves the window on every loss and grows it back by
//! one packet a round trip. With SCTP's small packets that ties a transfer to
//! the loss rate, whatever the line can do: at a 40 ms round trip, one packet
//! in a thousand lost at random (Wi-Fi, a long path) holds it near 1.5 MB/s.
//! BBR (Cardwell et al., "BBR: Congestion-Based Congestion Control", ACM
//! Queue 2016; draft-cardwell-iccrg-bbr-congestion-control) measures what
//! the path delivers instead: the highest delivery rate of the last ten round
//! trips and the lowest round trip of the last ten seconds. It sends at that
//! rate, paced, with at most twice their product in flight, and a loss only
//! costs the retransmission. This is BBR v1's state machine (Startup, Drain,
//! ProbeBW, ProbeRTT), with v2's exit from Startup when a round loses more
//! than 2% (a policer or a shallow queue, where going faster only loses more:
//! Cloudflare's TURN drops above a packet rate per allocation).
//!
//! The association feeds it a sample per SACK (delivery rate estimation,
//! draft-cheng-iccrg-delivery-rate-estimation) and asks it for the window
//! and the pacing rate; RACK-style loss detection (RFC 8985) lives with the
//! association, which has the chunks.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Startup's gain, 2/ln 2: the rate doubles every round trip.
const HIGH_GAIN: f64 = 2.885;
const DRAIN_GAIN: f64 = 1.0 / HIGH_GAIN;
const CWND_GAIN: f64 = 2.0;
/// ProbeBW's eight phases, one round trip each: probe up, drain what that
/// queued, cruise.
const CYCLE: [f64; 8] = [1.25, 0.75, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
/// The bandwidth filter's window, in round trips.
const BW_ROUNDS: u64 = 10;
/// The minimum round trip's window, and ProbeRTT's length.
const MIN_RTT_FOR: Duration = Duration::from_secs(10);
const PROBE_RTT_FOR: Duration = Duration::from_millis(200);
/// Startup ends after this many round trips without 25% more bandwidth.
const FULL_BW_GROWTH: f64 = 1.25;
const FULL_BW_ROUNDS: u32 = 3;
/// ... or after a round that lost more than this (and at least a few
/// packets).
const STARTUP_LOSS: f64 = 0.02;
const STARTUP_LOSS_PACKETS: u64 = 3;
/// The smallest window, in packets.
const MIN_CWND_PACKETS: u32 = 4;
/// The pacer lets this much time's worth go at once: what one wake-up of
/// the driver may send (tokio's timers are good to a millisecond or two
/// with the period raised, see the app's direct/link.rs).
const BURST: Duration = Duration::from_millis(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Startup,
    Drain,
    ProbeBw,
    ProbeRtt,
}

impl Mode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Mode::Startup => "startup",
            Mode::Drain => "drain",
            Mode::ProbeBw => "probe-bw",
            Mode::ProbeRtt => "probe-rtt",
        }
    }
}

/// What one SACK told: the association's delivered bytes after it, what it
/// delivered and what it showed lost, and the delivery rate it samples.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sample {
    /// All bytes delivered so far.
    pub(crate) delivered: u64,
    /// Newly delivered by this SACK.
    pub(crate) acked: u64,
    /// Newly found lost by this SACK.
    pub(crate) lost: u64,
    /// The `delivered` of the chunk the rate sample comes from (the most
    /// recently sent of those acknowledged), which counts round trips.
    pub(crate) prior_delivered: Option<u64>,
    /// Bytes a second, and whether the sender was short of data then.
    pub(crate) rate: Option<(f64, bool)>,
    /// A round trip measured on a chunk sent once.
    pub(crate) rtt: Option<Duration>,
    /// Bytes in flight after the SACK.
    pub(crate) inflight: u64,
}

pub(crate) struct Bbr {
    mode: Mode,
    mtu: u32,
    /// The highest delivery rate of each of the last BW_ROUNDS rounds.
    bw_rounds: VecDeque<(u64, f64)>,
    btl_bw: f64,
    min_rtt: Option<Duration>,
    min_rtt_stamp: Instant,
    round: u64,
    next_round_delivered: u64,
    round_start: bool,
    full_bw: f64,
    full_bw_rounds: u32,
    filled_pipe: bool,
    /// This round's losses and the delivered count it began at.
    round_lost: u64,
    round_began_at: u64,
    cycle_index: usize,
    cycle_stamp: Instant,
    probe_rtt_done: Option<Instant>,
    probe_rtt_round_done: bool,
    prior_cwnd: u32,
    /// After a retransmission timeout, until the next delivery.
    after_rto: bool,
    pacing_gain: f64,
    cwnd_gain: f64,
    pacing_rate: f64,
    cwnd: u32,
}

impl Bbr {
    pub(crate) fn new(now: Instant, mtu: u32, initial_cwnd: u32) -> Bbr {
        Bbr {
            mode: Mode::Startup,
            mtu,
            bw_rounds: VecDeque::new(),
            btl_bw: 0.0,
            min_rtt: None,
            min_rtt_stamp: now,
            round: 0,
            next_round_delivered: 0,
            round_start: false,
            full_bw: 0.0,
            full_bw_rounds: 0,
            filled_pipe: false,
            round_lost: 0,
            round_began_at: 0,
            cycle_index: 0,
            cycle_stamp: now,
            probe_rtt_done: None,
            probe_rtt_round_done: false,
            prior_cwnd: 0,
            after_rto: false,
            pacing_gain: HIGH_GAIN,
            cwnd_gain: HIGH_GAIN,
            // Until a sample: the first window in a millisecond, times the
            // gain (as Linux starts, without a round trip measured).
            pacing_rate: HIGH_GAIN * f64::from(initial_cwnd) / 1e-3,
            cwnd: initial_cwnd,
        }
    }

    pub(crate) fn cwnd(&self) -> u32 {
        self.cwnd
    }

    /// Bytes a second.
    pub(crate) fn pacing_rate(&self) -> f64 {
        self.pacing_rate
    }

    pub(crate) fn btl_bw(&self) -> f64 {
        self.btl_bw
    }

    pub(crate) fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    fn min_cwnd(&self) -> u32 {
        MIN_CWND_PACKETS * self.mtu
    }

    /// The path's bandwidth-delay product times `gain`, bytes.
    fn bdp(&self, gain: f64) -> Option<f64> {
        let rtt = self.min_rtt?;
        (self.btl_bw > 0.0).then(|| gain * self.btl_bw * rtt.as_secs_f64())
    }

    /// The window to aim for: `gain` BDPs and room for the packets one
    /// wake-up sends at once (Linux adds three send quanta).
    fn target_cwnd(&self, gain: f64) -> Option<u32> {
        let bdp = self.bdp(gain)?;
        let quanta = 3.0 * (self.pacing_rate * BURST.as_secs_f64()).max(f64::from(2 * self.mtu));
        Some(((bdp + quanta) as u32).max(self.min_cwnd()))
    }

    /// A retransmission timeout: the window shrinks to the minimum until
    /// something is delivered again; the model stays (the path's rate did
    /// not change because a packet was lost).
    pub(crate) fn on_rto(&mut self) {
        if !self.after_rto {
            self.prior_cwnd = self.prior_cwnd.max(self.cwnd);
        }
        self.after_rto = true;
        self.cwnd = self.min_cwnd();
    }

    pub(crate) fn on_sack(&mut self, now: Instant, s: &Sample) {
        // Round trips, counted by delivery: a round ends when a chunk sent
        // after the last one began is acknowledged.
        self.round_start = false;
        if let Some(prior) = s.prior_delivered
            && prior >= self.next_round_delivered
        {
            self.next_round_delivered = s.delivered;
            self.round += 1;
            self.round_start = true;
        }
        self.round_lost += s.lost;
        let mut round_lossy = false;
        if self.round_start {
            let delivered = s.delivered.saturating_sub(self.round_began_at);
            let sent = delivered + self.round_lost;
            round_lossy = self.round_lost >= STARTUP_LOSS_PACKETS * u64::from(self.mtu)
                && sent > 0
                && self.round_lost as f64 > STARTUP_LOSS * sent as f64;
            self.round_lost = 0;
            self.round_began_at = s.delivered;
        }

        // The bandwidth filter: a round's best, kept for BW_ROUNDS rounds. A
        // sample while short of data only counts if it is higher.
        if let Some((rate, app_limited)) = s.rate
            && (!app_limited || rate >= self.btl_bw)
        {
            match self.bw_rounds.back_mut() {
                Some((round, best)) if *round == self.round => *best = best.max(rate),
                _ => self.bw_rounds.push_back((self.round, rate)),
            }
        }
        while self
            .bw_rounds
            .front()
            .is_some_and(|(round, _)| round + BW_ROUNDS <= self.round)
        {
            self.bw_rounds.pop_front();
        }
        self.btl_bw = self.bw_rounds.iter().map(|(_, b)| *b).fold(0.0, f64::max);

        // The minimum round trip, renewed every MIN_RTT_FOR (a lower one
        // always counts; as Linux's BBR, an equal one does not renew it).
        let rtt_expired = now.duration_since(self.min_rtt_stamp) > MIN_RTT_FOR;
        if let Some(rtt) = s.rtt
            && (self.min_rtt.is_none_or(|m| rtt < m) || rtt_expired)
        {
            self.min_rtt = Some(rtt);
            self.min_rtt_stamp = now;
        }

        self.update_mode(now, s, round_lossy, rtt_expired);
        self.set_pacing_rate();
        self.set_cwnd(s);
    }

    fn update_mode(&mut self, now: Instant, s: &Sample, round_lossy: bool, rtt_expired: bool) {
        // Startup: until the bandwidth stops growing, or a round loses.
        if self.mode == Mode::Startup && self.round_start {
            if self.btl_bw >= self.full_bw * FULL_BW_GROWTH {
                self.full_bw = self.btl_bw;
                self.full_bw_rounds = 0;
            } else if !s.rate.is_some_and(|(_, app_limited)| app_limited) {
                self.full_bw_rounds += 1;
            }
            if self.full_bw_rounds >= FULL_BW_ROUNDS || (round_lossy && self.btl_bw > 0.0) {
                self.filled_pipe = true;
                self.mode = Mode::Drain;
                self.pacing_gain = DRAIN_GAIN;
                self.cwnd_gain = HIGH_GAIN;
            }
        }
        // Drain: until what Startup queued is gone.
        if self.mode == Mode::Drain && self.bdp(1.0).is_some_and(|bdp| s.inflight as f64 <= bdp) {
            self.enter_probe_bw(now);
        }
        // ProbeBW: a phase a round trip long; probing up goes on until the
        // extra is in flight (or lost), draining until the queue is gone.
        if self.mode == Mode::ProbeBw
            && let Some(rtt) = self.min_rtt
        {
            let full = now.duration_since(self.cycle_stamp) > rtt;
            let gain = CYCLE[self.cycle_index];
            let next = if gain > 1.0 {
                full && (s.lost > 0 || self.bdp(gain).is_some_and(|t| s.inflight as f64 >= t))
            } else if gain < 1.0 {
                full || self.bdp(1.0).is_some_and(|t| s.inflight as f64 <= t)
            } else {
                full
            };
            if next {
                self.cycle_index = (self.cycle_index + 1) % CYCLE.len();
                self.cycle_stamp = now;
                self.pacing_gain = CYCLE[self.cycle_index];
            }
        }
        // ProbeRTT: every MIN_RTT_FOR without a lower round trip, a few
        // packets in flight for PROBE_RTT_FOR and a round, to see the path
        // empty.
        if self.mode != Mode::ProbeRtt && rtt_expired && self.min_rtt.is_some() {
            self.mode = Mode::ProbeRtt;
            self.pacing_gain = 1.0;
            self.cwnd_gain = 1.0;
            self.prior_cwnd = self.prior_cwnd.max(self.cwnd);
            self.probe_rtt_done = None;
        }
        if self.mode == Mode::ProbeRtt {
            match self.probe_rtt_done {
                None if s.inflight <= u64::from(self.min_cwnd()) => {
                    self.probe_rtt_done = Some(now + PROBE_RTT_FOR);
                    self.probe_rtt_round_done = false;
                    self.next_round_delivered = s.delivered;
                }
                Some(done) => {
                    if self.round_start {
                        self.probe_rtt_round_done = true;
                    }
                    if self.probe_rtt_round_done && now >= done {
                        self.min_rtt_stamp = now;
                        self.cwnd = self.cwnd.max(self.prior_cwnd);
                        self.prior_cwnd = 0;
                        if self.filled_pipe {
                            self.enter_probe_bw(now);
                        } else {
                            self.mode = Mode::Startup;
                            self.pacing_gain = HIGH_GAIN;
                            self.cwnd_gain = HIGH_GAIN;
                        }
                    }
                }
                None => {}
            }
        }
    }

    fn enter_probe_bw(&mut self, now: Instant) {
        self.mode = Mode::ProbeBw;
        self.cwnd_gain = CWND_GAIN;
        // Not in the draining phase: a cruising one, so the first probe
        // comes within the cycle.
        self.cycle_index = 2;
        self.cycle_stamp = now;
        self.pacing_gain = CYCLE[self.cycle_index];
    }

    fn set_pacing_rate(&mut self) {
        if self.btl_bw <= 0.0 {
            return;
        }
        let rate = self.pacing_gain * self.btl_bw;
        // While Startup fills the pipe the rate only goes up.
        if self.filled_pipe || rate > self.pacing_rate {
            self.pacing_rate = rate;
        }
    }

    fn set_cwnd(&mut self, s: &Sample) {
        if s.acked > 0 && self.after_rto {
            // Delivering again: back to where the window was.
            self.after_rto = false;
            self.cwnd = self.cwnd.max(self.prior_cwnd);
            self.prior_cwnd = 0;
        }
        if let Some(target) = self.target_cwnd(self.cwnd_gain) {
            let grown = self.cwnd.saturating_add(s.acked as u32);
            self.cwnd = if self.filled_pipe {
                grown.min(target)
            } else if self.cwnd < target {
                grown
            } else {
                self.cwnd
            };
        } else if !self.filled_pipe {
            // No model yet: grow as slow start does.
            self.cwnd = self.cwnd.saturating_add(s.acked as u32);
        }
        self.cwnd = self.cwnd.max(self.min_cwnd());
        if self.mode == Mode::ProbeRtt || self.after_rto {
            self.cwnd = self.cwnd.min(self.min_cwnd());
        }
    }
}

/// Lets data go at the pacing rate: credit grows with time, up to BURST's
/// worth, and each packet spends its size.
#[derive(Debug, Default)]
pub(crate) struct Pacer {
    credit: f64,
    last: Option<Instant>,
}

impl Pacer {
    /// Whether `len` more bytes may go now at `rate` bytes a second; when
    /// not, when they may.
    pub(crate) fn admit(
        &mut self,
        now: Instant,
        rate: f64,
        mtu: u32,
        len: usize,
    ) -> Result<(), Instant> {
        let cap = (rate * BURST.as_secs_f64()).max(f64::from(4 * mtu));
        self.credit = match self.last {
            Some(last) => (self.credit + now.duration_since(last).as_secs_f64() * rate).min(cap),
            None => cap,
        };
        self.last = Some(now);
        if self.credit > 0.0 {
            self.credit -= len as f64;
            Ok(())
        } else {
            let wait = (-self.credit + 1.0) / rate.max(1.0);
            Err(now + Duration::from_secs_f64(wait.min(1.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: u32 = 1191;

    /// Feeds a path of `rate` bytes a second and `rtt` (nothing lost, never
    /// short of data) for `rounds` round trips, a SACK a millisecond.
    fn run(
        bbr: &mut Bbr,
        start: Instant,
        rate: f64,
        rtt: Duration,
        rounds: u32,
        lost_per_round: u64,
    ) -> Instant {
        let mut now = start;
        let mut delivered = 0u64;
        let step = Duration::from_millis(1);
        let per_step = (rate * step.as_secs_f64()) as u64;
        let steps = (rtt.as_millis() as u32).max(1);
        for _ in 0..rounds {
            let round_began = delivered;
            for i in 0..steps {
                now += step;
                delivered += per_step;
                let s = Sample {
                    delivered,
                    acked: per_step,
                    lost: if i == 0 { lost_per_round } else { 0 },
                    prior_delivered: Some(round_began + u64::from(i) * per_step),
                    rate: Some((rate, false)),
                    rtt: Some(rtt),
                    inflight: (rate * rtt.as_secs_f64()) as u64,
                };
                bbr.on_sack(now, &s);
            }
        }
        now
    }

    #[test]
    fn startup_finds_the_rate_and_cruises_there() {
        let t = Instant::now();
        let mut bbr = Bbr::new(t, MTU, 4380);
        let rate = 12.5e6; // 100 Mbit/s
        let rtt = Duration::from_millis(40);
        run(&mut bbr, t, rate, rtt, 30, 0);
        assert!(bbr.filled_pipe);
        assert_eq!(bbr.mode(), Mode::ProbeBw);
        assert!((bbr.btl_bw() - rate).abs() < 1.0);
        // About the rate (within the probing cycle's gains), and two BDPs.
        assert!(bbr.pacing_rate() >= 0.75 * rate && bbr.pacing_rate() <= 1.25 * rate);
        let bdp = rate * rtt.as_secs_f64();
        assert!(f64::from(bbr.cwnd()) > 1.9 * bdp && f64::from(bbr.cwnd()) < 2.5 * bdp);
    }

    #[test]
    fn random_loss_does_not_shrink_anything() {
        let t = Instant::now();
        let mut bbr = Bbr::new(t, MTU, 4380);
        let rate = 12.5e6;
        let rtt = Duration::from_millis(40);
        let now = run(&mut bbr, t, rate, rtt, 30, 0);
        let before = bbr.cwnd();
        // One packet lost in a thousand, every round.
        run(&mut bbr, now, rate, rtt, 50, u64::from(MTU));
        assert!(bbr.cwnd() >= before * 9 / 10, "{} {}", bbr.cwnd(), before);
        assert!(bbr.pacing_rate() >= 0.75 * rate);
    }

    #[test]
    fn a_lossy_startup_stops_growing() {
        let t = Instant::now();
        let mut bbr = Bbr::new(t, MTU, 4380);
        // Each round loses 10%: a policer.
        let rate = 6e6;
        let rtt = Duration::from_millis(20);
        let per_round = (rate * rtt.as_secs_f64()) as u64;
        run(&mut bbr, t, rate, rtt, 6, per_round / 10);
        assert!(bbr.filled_pipe);
        assert_ne!(bbr.mode(), Mode::Startup);
    }

    #[test]
    fn probe_rtt_comes_every_ten_seconds_and_goes() {
        let t = Instant::now();
        let mut bbr = Bbr::new(t, MTU, 4380);
        let rate = 5e6;
        let rtt = Duration::from_millis(50);
        // Over ten seconds at the same round trip.
        let now = run(&mut bbr, t, rate, rtt, 205, 0);
        let mut seen = false;
        let mut now2 = now;
        for _ in 0..20 {
            seen |= bbr.mode() == Mode::ProbeRtt;
            // In ProbeRTT the flight empties: report it so.
            let s = Sample {
                delivered: 1 << 40,
                acked: 1000,
                lost: 0,
                prior_delivered: Some(1 << 40),
                rate: Some((rate, false)),
                rtt: Some(rtt),
                inflight: 1000,
            };
            now2 += Duration::from_millis(50);
            bbr.on_sack(now2, &s);
        }
        assert!(seen);
        assert_eq!(bbr.mode(), Mode::ProbeBw);
    }

    #[test]
    fn a_timeout_shrinks_the_window_until_delivery() {
        let t = Instant::now();
        let mut bbr = Bbr::new(t, MTU, 4380);
        let now = run(&mut bbr, t, 12.5e6, Duration::from_millis(40), 30, 0);
        let before = bbr.cwnd();
        bbr.on_rto();
        assert_eq!(bbr.cwnd(), 4 * MTU);
        run(&mut bbr, now, 12.5e6, Duration::from_millis(40), 1, 0);
        assert!(bbr.cwnd() >= before * 9 / 10);
    }

    #[test]
    fn the_pacer_spreads_a_burst() {
        let t = Instant::now();
        let mut p = Pacer::default();
        let rate = 1e6; // a byte a microsecond
        // The first wake-up may send BURST's worth (4 KB at this rate, but
        // at least four packets).
        let mut sent = 0;
        while p.admit(t, rate, MTU, 1000).is_ok() {
            sent += 1000;
        }
        assert!(
            (4 * MTU as usize..=4 * MTU as usize + 1000).contains(&sent),
            "{sent}"
        );
        // Then a packet a millisecond.
        let Err(when) = p.admit(t, rate, MTU, 1000) else {
            panic!("should wait");
        };
        assert!(when > t && when <= t + Duration::from_millis(2));
        assert!(
            p.admit(t + Duration::from_millis(2), rate, MTU, 1000)
                .is_ok()
        );
    }
}
