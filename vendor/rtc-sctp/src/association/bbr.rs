//! BBR congestion control for the sending side, and its pacer (Babu Tela).
//!
//! Reno (RFC 9260 §7.2) halves the window on every loss and grows it back by
//! one packet a round trip. With SCTP's small packets that ties a transfer to
//! the loss rate, whatever the line can do: at a 40 ms round trip, one packet
//! in a thousand lost at random (Wi-Fi, a long path) holds it near 1.5 MB/s.
//! BBR measures what the path delivers instead (the highest delivery rate,
//! the lowest round trip) and sends at that rate, paced: a loss costs its
//! retransmission, not the window.
//!
//! This is BBRv3 (draft-ietf-ccwg-bbr-06): Startup, Drain, ProbeBW in four
//! phases (DOWN, CRUISE, REFILL, UP; a probe every 2 to 3 s, or every 63
//! rounds with a window of more packets than that) and ProbeRTT, with v3's
//! answers to loss. BBRv1, here before, kept twice the path's capacity in
//! flight whatever was lost: through a queue shorter than that it lost
//! packets all the time (3.9 % of a transfer's at 100 Mbit/s with a 5 ms
//! queue), and a call on the same upload lost its voice with them (10.8 %
//! at 2 Mbit/s with a 20 ms queue). v3 bounds what is in flight by what was
//! in flight when a probe lost more than 2 % (`inflight_longterm`, cruising
//! 15 % under it); when not probing, a round with losses slows it toward
//! the latest delivery (`bw_shortterm`, `inflight_shortterm`); and a lossy
//! round ends Startup with the bound set. Random loss under 2 % does not
//! move the bound. Measured on the same lines (`cargo xtask direto`, three
//! rounds each): 0.11 % resent at 100 Mbit/s with a 5 ms queue, at 10.7
//! MB/s (v1: 11.3); the call's voice lost 0.1 % at 20 Mbit/s with a 20 ms
//! queue (v1: 2.5 %) and 1.5 % at 2 Mbit/s (v1: 10.8 %; Reno and CUBIC
//! 2.9 %), the file as fast as with v1; and 72 MB/s still at a 40 ms round
//! trip losing 0.1 %.
//!
//! Left out of the draft: ECN, the allowance for ACK aggregation
//! (`extra_acked`), undoing a recovery found spurious, and the restart from
//! idle. Startup's loss exit counts lost packets, not lost ranges. One
//! change, for a call sharing the upload: a window under 63 packets probes
//! on the 2 to 3 s clock alone. The draft probes it every window's worth of
//! rounds (to grow as Reno would), which for the 8 packets of a 2 Mbit/s
//! upload at 40 ms is every 0.4 s or so, and each probe overflows a short
//! queue: a call on that upload with a 20 ms queue lost 4.0 % of its
//! voice beside a file (and the file resent 2.9 %), against 0.8 % (and
//! 0.6 %) with this change, at the same speed; four slides lost it 4.5 %
//! against 1.1 % (three rounds of each, interleaved).
//!
//! The association feeds it a sample per SACK (delivery rate estimation,
//! draft-cheng-iccrg-delivery-rate-estimation) and every chunk RACK finds
//! lost (RFC 8985), and asks it for the window and the pacing rate.

use std::time::{Duration, Instant};

/// Startup's pacing gain, 4 ln 2: the rate doubles every round trip.
const STARTUP_PACING_GAIN: f64 = 2.77;
/// Drain's: what Startup queued goes in about a round trip.
const DRAIN_PACING_GAIN: f64 = 0.5;
/// What may be in flight, in BDPs, except in ProbeBW_UP and ProbeRTT.
const DEFAULT_CWND_GAIN: f64 = 2.0;
const DOWN_PACING_GAIN: f64 = 0.9;
const UP_PACING_GAIN: f64 = 1.25;
const UP_CWND_GAIN: f64 = 2.25;
const PROBE_RTT_CWND_GAIN: f64 = 0.5;
/// Paced a little under the estimate, so what queues drains.
const PACING_MARGIN: f64 = 0.01;
/// Losses above this share of what was in flight mean too much in flight.
const LOSS_THRESH: f64 = 0.02;
/// What a loss leaves (as CUBIC's 0.7).
const BETA: f64 = 0.7;
/// The room left in the bottleneck's queue while cruising.
const HEADROOM: f64 = 0.15;
/// The full-pipe estimator: three rounds without 25 % more bandwidth.
const FULL_BW_GROWTH: f64 = 1.25;
const FULL_BW_ROUNDS: u32 = 3;
/// Startup also ends after a round that lost more than LOSS_THRESH of what
/// it sent, and at least this many packets.
const STARTUP_LOSS_PACKETS: u64 = 3;
/// Drain ends after this many rounds, whatever is still in flight.
const DRAIN_MAX_ROUNDS: u64 = 3;
/// The smallest window, in packets.
const MIN_PIPE_PACKETS: u32 = 4;
/// The minimum round trip's window. ProbeRTT comes when no round trip as
/// low came for PROBE_RTT_INTERVAL, and lasts PROBE_RTT_DURATION and a
/// round.
const MIN_RTT_FILTER: Duration = Duration::from_secs(10);
const PROBE_RTT_INTERVAL: Duration = Duration::from_secs(5);
const PROBE_RTT_DURATION: Duration = Duration::from_millis(200);
/// ProbeBW probes after 2 to 3 s of DOWN and CRUISE, or after as many
/// rounds as the window has packets (as Reno would grow it), 63 at most.
const PROBE_WAIT: Duration = Duration::from_secs(2);
const RENO_ROUNDS_MAX: u64 = 63;
/// The pacer lets this much time's worth go at once: what one wake-up of
/// the driver may send (tokio's timers are good to a millisecond or two
/// with the period raised, see the app's direct/link.rs).
const BURST: Duration = Duration::from_millis(4);

/// The draft's PickProbeWait: 0 or 1 rounds counted already, and 2 to 3 s,
/// at random, so flows that meet in a queue do not probe in step. Fixed in
/// the tests, which must come out the same every time.
fn pick_probe_wait() -> (u64, Duration) {
    if cfg!(test) {
        (0, PROBE_WAIT + Duration::from_millis(500))
    } else {
        (
            u64::from(rand::random::<bool>()),
            PROBE_WAIT + Duration::from_secs_f64(rand::random::<f64>()),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Startup,
    Drain,
    /// ProbeBW: slowing down after a probe, until the queue is gone.
    Down,
    /// ProbeBW: at the estimate, until it is time to probe.
    Cruise,
    /// ProbeBW: a round at the estimate with the bounds lifted, before UP.
    Refill,
    /// ProbeBW: faster, to find more bandwidth or the most in flight.
    Up,
    ProbeRtt,
}

impl Mode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Mode::Startup => "startup",
            Mode::Drain => "drain",
            Mode::Down => "down",
            Mode::Cruise => "cruise",
            Mode::Refill => "refill",
            Mode::Up => "up",
            Mode::ProbeRtt => "probe-rtt",
        }
    }

    fn probe_bw(self) -> bool {
        matches!(self, Mode::Down | Mode::Cruise | Mode::Refill | Mode::Up)
    }

    /// Accelerating: the answers to loss are the probe's, not the
    /// short-term model's.
    fn probing(self) -> bool {
        matches!(self, Mode::Startup | Mode::Refill | Mode::Up)
    }
}

/// Where the feedback of a bandwidth probe stands (the draft's ack_phase).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckPhase {
    Init,
    Refilling,
    ProbeStarting,
    ProbeFeedback,
    ProbeStopping,
}

/// What one SACK told: the association's delivered bytes after it, what it
/// delivered and what it showed lost, and the rate sample of the newest
/// chunk it acknowledged.
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
    /// Bytes delivered since that chunk went out (the draft's RS.delivered).
    pub(crate) sample_delivered: u64,
    /// Bytes in flight when that chunk went out, and lost since.
    pub(crate) tx_in_flight: Option<u64>,
    pub(crate) lost_since: u64,
    /// A round trip measured on a chunk sent once.
    pub(crate) rtt: Option<Duration>,
    /// Bytes in flight after the SACK.
    pub(crate) inflight: u64,
    /// The window held sending back since the last SACK.
    pub(crate) cwnd_limited: bool,
}

/// A chunk found lost, as it was sent.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Lost {
    pub(crate) size: u64,
    /// Bytes in flight when it went out (itself included).
    pub(crate) tx_in_flight: u64,
    /// Bytes found lost since it went out (itself included).
    pub(crate) lost_since: u64,
    pub(crate) app_limited: bool,
}

pub(crate) struct Bbr {
    mode: Mode,
    mtu: u32,
    initial_cwnd: u32,

    /// The highest delivery rate of the previous ProbeBW cycle and of this
    /// one (in Startup, of everything so far), and their maximum.
    bw_hi: [f64; 2],
    max_bw: f64,
    /// The short-term model: slowed toward the latest delivery after lossy
    /// rounds when not probing (infinite: no bound).
    bw_shortterm: f64,
    inflight_shortterm: f64,
    /// `max_bw` within the short-term bound: what the pacing and the BDP
    /// use.
    bw: f64,
    /// This round's highest delivery rate and volume.
    bw_latest: f64,
    inflight_latest: u64,
    /// The most in flight before losses passed LOSS_THRESH (infinite until
    /// they have).
    inflight_longterm: f64,

    min_rtt: Option<Duration>,
    min_rtt_stamp: Instant,
    probe_rtt_min_delay: Option<Duration>,
    probe_rtt_min_stamp: Instant,
    probe_rtt_expired: bool,

    /// Round trips, counted by delivery.
    delivered: u64,
    round: u64,
    next_round_delivered: u64,
    round_start: bool,
    rounds_since_probe_up: u64,
    /// Losses in this round (bytes, for Startup's exit), and whether any.
    round_lost: u64,
    round_began_at: u64,
    loss_in_round: bool,

    full_bw: f64,
    full_bw_count: u32,
    full_bw_now: bool,
    full_bw_reached: bool,
    drain_start_round: u64,

    ack_phase: AckPhase,
    cycle_stamp: Instant,
    bw_probe_wait: Duration,
    /// ProbeBW_UP raises `inflight_longterm` a packet per this many bytes
    /// acknowledged, the step doubling every round.
    probe_up_acked_per_inc: f64,
    bw_probe_up_acked: u64,
    bw_probe_up_rounds: u32,
    /// Losses now count against the probe that sent them.
    is_bw_probe_sample: bool,
    prev_probe_too_high: bool,
    prev_probe_precautionary: bool,

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
            initial_cwnd,
            bw_hi: [0.0; 2],
            max_bw: 0.0,
            bw_shortterm: f64::INFINITY,
            inflight_shortterm: f64::INFINITY,
            bw: 0.0,
            bw_latest: 0.0,
            inflight_latest: 0,
            inflight_longterm: f64::INFINITY,
            min_rtt: None,
            min_rtt_stamp: now,
            probe_rtt_min_delay: None,
            probe_rtt_min_stamp: now,
            probe_rtt_expired: false,
            delivered: 0,
            round: 0,
            next_round_delivered: 0,
            round_start: false,
            rounds_since_probe_up: 0,
            round_lost: 0,
            round_began_at: 0,
            loss_in_round: false,
            full_bw: 0.0,
            full_bw_count: 0,
            full_bw_now: false,
            full_bw_reached: false,
            drain_start_round: 0,
            ack_phase: AckPhase::Init,
            cycle_stamp: now,
            bw_probe_wait: Duration::ZERO,
            probe_up_acked_per_inc: f64::INFINITY,
            bw_probe_up_acked: 0,
            bw_probe_up_rounds: 0,
            is_bw_probe_sample: false,
            prev_probe_too_high: false,
            prev_probe_precautionary: false,
            probe_rtt_done: None,
            probe_rtt_round_done: false,
            prior_cwnd: 0,
            after_rto: false,
            pacing_gain: STARTUP_PACING_GAIN,
            cwnd_gain: DEFAULT_CWND_GAIN,
            // Until a round trip is measured: the first window in a
            // millisecond, times the gain (as Linux starts without one).
            pacing_rate: STARTUP_PACING_GAIN * f64::from(initial_cwnd) / 1e-3,
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

    /// The path's bandwidth as measured (bytes a second).
    pub(crate) fn btl_bw(&self) -> f64 {
        self.max_bw
    }

    pub(crate) fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// The bound loss set on what is in flight, bytes (None: none yet).
    pub(crate) fn inflight_longterm(&self) -> Option<f64> {
        self.inflight_longterm
            .is_finite()
            .then_some(self.inflight_longterm)
    }

    fn min_pipe(&self) -> f64 {
        f64::from(MIN_PIPE_PACKETS * self.mtu)
    }

    /// `gain` times the path's bandwidth-delay product, bytes.
    fn bdp(&self, gain: f64) -> Option<f64> {
        let rtt = self.min_rtt?;
        (self.bw > 0.0).then_some(gain * self.bw * rtt.as_secs_f64())
    }

    /// `gain` BDPs and room for the packets one wake-up sends at once
    /// (three bursts, as Linux), two packets more while probing up.
    fn inflight(&self, gain: f64) -> Option<f64> {
        let bdp = self.bdp(gain)?;
        let quanta = 3.0 * (self.pacing_rate * BURST.as_secs_f64()).max(f64::from(2 * self.mtu));
        let up = if self.mode == Mode::Up {
            f64::from(2 * self.mtu)
        } else {
            0.0
        };
        Some((bdp + quanta).max(self.min_pipe()) + up)
    }

    /// What it aims to have in flight: a BDP, unless losses cut the window.
    fn target_inflight(&self) -> f64 {
        let cwnd = f64::from(self.cwnd);
        self.bdp(1.0).map_or(cwnd, |bdp| bdp.min(cwnd))
    }

    /// The long-term bound less the room left for others in the queue.
    fn inflight_with_headroom(&self) -> f64 {
        if self.inflight_longterm.is_infinite() {
            return f64::INFINITY;
        }
        let headroom = f64::from(self.mtu).max(HEADROOM * self.inflight_longterm);
        (self.inflight_longterm - headroom).max(self.min_pipe())
    }

    /// A retransmission timeout: the window shrinks to the minimum until
    /// something is delivered again; the model stays (the path's rate did
    /// not change because a packet was lost).
    pub(crate) fn on_rto(&mut self) {
        self.save_cwnd();
        self.after_rto = true;
        self.loss_in_round = true;
        self.cwnd = MIN_PIPE_PACKETS * self.mtu;
    }

    /// A chunk RACK found lost (the draft's HandleLostPacket): when it went
    /// out in a probe and more than LOSS_THRESH of what was in flight then
    /// has been lost since, that much in flight was too much.
    pub(crate) fn on_lost(&mut self, now: Instant, p: Lost) {
        self.loss_in_round = true;
        if !self.is_bw_probe_sample {
            return;
        }
        let (tx, lost) = (p.tx_in_flight as f64, p.lost_since as f64);
        if lost <= tx * LOSS_THRESH {
            return;
        }
        // Where, within this chunk, losses passed the threshold: what was
        // in flight before it, and the part of it that fitted.
        let size = p.size as f64;
        let inflight_prev = tx - size;
        let lost_prev = lost - size;
        // As Linux's BBRv3 has it: when the losses before this chunk had
        // passed the threshold already, the level is what was in flight
        // before it; counts that cannot be (the chunk not counted in its
        // own flight) set no bound.
        let at_loss = if inflight_prev < 0.0 || lost_prev < 0.0 {
            f64::INFINITY
        } else {
            let lost_prefix = (LOSS_THRESH * inflight_prev - lost_prev) / (1.0 - LOSS_THRESH);
            inflight_prev + lost_prefix.max(0.0)
        };
        self.inflight_too_high(now, at_loss, p.app_limited);
    }

    /// The draft's HandleInflightTooHigh.
    fn inflight_too_high(&mut self, now: Instant, tx_in_flight: f64, app_limited: bool) {
        self.prev_probe_too_high = true;
        // Once per probe.
        self.is_bw_probe_sample = false;
        if !app_limited {
            self.inflight_longterm = tx_in_flight.max(self.target_inflight() * BETA).floor();
        }
        if self.mode == Mode::Up {
            self.start_down(now);
        }
    }

    pub(crate) fn on_sack(&mut self, now: Instant, s: &Sample) {
        self.delivered = s.delivered;
        // Round trips, counted by delivery: a round ends when a chunk sent
        // after the last one began is acknowledged.
        self.round_start = false;
        if let Some(prior) = s.prior_delivered
            && prior >= self.next_round_delivered
        {
            self.start_round();
            self.round += 1;
            self.rounds_since_probe_up += 1;
            self.round_start = true;
        }
        let rate = s.rate.map(|(r, _)| r);
        let app_limited = s.rate.is_some_and(|(_, a)| a);
        // This round's delivery so far.
        self.bw_latest = self.bw_latest.max(rate.unwrap_or(0.0));
        self.inflight_latest = self.inflight_latest.max(s.sample_delivered);
        // Startup's loss exit: what the round lost against what it sent.
        self.round_lost += s.lost;
        let mut round_lossy = false;
        if self.round_start {
            let delivered = s.delivered.saturating_sub(self.round_began_at);
            let sent = delivered + self.round_lost;
            round_lossy = self.round_lost >= STARTUP_LOSS_PACKETS * u64::from(self.mtu)
                && sent > 0
                && self.round_lost as f64 > LOSS_THRESH * sent as f64;
            self.round_lost = 0;
            self.round_began_at = s.delivered;
        }

        // The bandwidth: the highest of this cycle and the last. A sample
        // while short of data only counts if it is higher.
        if let Some(r) = rate
            && (r >= self.max_bw || !app_limited)
        {
            self.bw_hi[1] = self.bw_hi[1].max(r);
            self.max_bw = self.bw_hi[0].max(self.bw_hi[1]);
        }
        // Once a round, when not probing, a lossy round slows the
        // short-term model toward what the round delivered.
        if self.round_start {
            if !self.mode.probing() && self.loss_in_round {
                if self.bw_shortterm.is_infinite() {
                    self.bw_shortterm = self.max_bw;
                }
                if self.inflight_shortterm.is_infinite() {
                    self.inflight_shortterm = f64::from(self.cwnd);
                }
                self.bw_shortterm = self.bw_latest.max(BETA * self.bw_shortterm);
                self.inflight_shortterm = (self.inflight_latest as f64)
                    .max(BETA * self.inflight_shortterm)
                    .floor();
            }
            self.loss_in_round = false;
        }

        self.check_full_bw(s, rate, app_limited);
        self.check_startup_done(round_lossy);
        if self.mode == Mode::Drain
            && (self
                .inflight(1.0)
                .is_some_and(|bdp| s.inflight as f64 <= bdp)
                || self.round > self.drain_start_round + DRAIN_MAX_ROUNDS)
        {
            self.cwnd_gain = DEFAULT_CWND_GAIN;
            self.start_down(now);
        }
        if s.acked > 0 {
            self.update_probe_bw(now, s, rate, app_limited);
        }
        self.update_min_rtt(now, s);
        self.check_probe_rtt(now, s);
        // A new round starts its delivery signals from this sample.
        if self.round_start {
            self.bw_latest = rate.unwrap_or(0.0);
            self.inflight_latest = s.sample_delivered;
        }
        self.bw = self.max_bw.min(self.bw_shortterm);

        self.set_pacing_rate();
        self.set_cwnd(s);
    }

    fn start_round(&mut self) {
        self.next_round_delivered = self.delivered;
    }

    fn reset_full_bw(&mut self) {
        self.full_bw = 0.0;
        self.full_bw_count = 0;
        self.full_bw_now = false;
    }

    /// The full-pipe estimator (Startup and ProbeBW_UP): three rounds whose
    /// samples, not short of data, came under 25 % above the baseline.
    fn check_full_bw(&mut self, s: &Sample, rate: Option<f64>, app_limited: bool) {
        let Some(rate) = rate else {
            return;
        };
        if self.full_bw_now || app_limited || s.acked == 0 {
            return;
        }
        if rate >= self.full_bw * FULL_BW_GROWTH {
            self.reset_full_bw();
            self.full_bw = rate;
            return;
        }
        if !self.round_start {
            return;
        }
        self.full_bw_count += 1;
        self.full_bw_now = self.full_bw_count >= FULL_BW_ROUNDS;
        self.full_bw_reached |= self.full_bw_now;
    }

    fn check_startup_done(&mut self, round_lossy: bool) {
        if self.mode != Mode::Startup {
            return;
        }
        // A round lost too much: the pipe is full, and about what it
        // carried then is the most to have in flight.
        if round_lossy && self.max_bw > 0.0 {
            self.full_bw_reached = true;
            let bdp = self.bdp(1.0).unwrap_or(0.0);
            self.inflight_longterm = bdp.max(self.inflight_latest as f64).floor();
        }
        if self.full_bw_reached {
            self.mode = Mode::Drain;
            self.pacing_gain = DRAIN_PACING_GAIN;
            self.cwnd_gain = DEFAULT_CWND_GAIN;
            self.drain_start_round = self.round;
        }
    }

    // ------------------------------------------------------------ ProbeBW

    fn start_down(&mut self, now: Instant) {
        // ResetCongestionSignals.
        self.loss_in_round = false;
        self.bw_latest = 0.0;
        self.inflight_latest = 0;
        self.probe_up_acked_per_inc = f64::INFINITY;
        (self.rounds_since_probe_up, self.bw_probe_wait) = pick_probe_wait();
        self.cycle_stamp = now;
        self.ack_phase = AckPhase::ProbeStopping;
        self.start_round();
        self.mode = Mode::Down;
        self.pacing_gain = DOWN_PACING_GAIN;
        self.cwnd_gain = DEFAULT_CWND_GAIN;
    }

    fn start_cruise(&mut self) {
        self.mode = Mode::Cruise;
        self.pacing_gain = 1.0;
        self.cwnd_gain = DEFAULT_CWND_GAIN;
    }

    fn start_refill(&mut self) {
        self.bw_shortterm = f64::INFINITY;
        self.inflight_shortterm = f64::INFINITY;
        self.bw_probe_up_rounds = 0;
        self.bw_probe_up_acked = 0;
        self.prev_probe_precautionary = false;
        self.ack_phase = AckPhase::Refilling;
        self.start_round();
        self.mode = Mode::Refill;
        self.pacing_gain = 1.0;
        self.cwnd_gain = DEFAULT_CWND_GAIN;
    }

    fn start_up(&mut self, rate: Option<f64>) {
        self.ack_phase = AckPhase::ProbeStarting;
        self.start_round();
        self.reset_full_bw();
        self.full_bw = rate.unwrap_or(0.0);
        self.mode = Mode::Up;
        self.pacing_gain = UP_PACING_GAIN;
        self.cwnd_gain = UP_CWND_GAIN;
        self.raise_inflight_longterm_slope();
    }

    /// ProbeBW_UP raises the bound by 1, 2, 4... packets a round.
    fn raise_inflight_longterm_slope(&mut self) {
        let growth = f64::from(1u32 << self.bw_probe_up_rounds);
        self.bw_probe_up_rounds = (self.bw_probe_up_rounds + 1).min(30);
        self.probe_up_acked_per_inc = (f64::from(self.cwnd) / growth).max(f64::from(self.mtu));
    }

    fn probe_inflight_longterm_upward(&mut self, s: &Sample) {
        if !s.cwnd_limited || f64::from(self.cwnd) < self.inflight_longterm {
            // Not using all the bound: nothing to learn by raising it.
            return;
        }
        self.bw_probe_up_acked += s.acked;
        if self.bw_probe_up_acked as f64 >= self.probe_up_acked_per_inc {
            let steps = (self.bw_probe_up_acked as f64 / self.probe_up_acked_per_inc).floor();
            self.bw_probe_up_acked -= (steps * self.probe_up_acked_per_inc) as u64;
            self.inflight_longterm += steps * f64::from(self.mtu);
        }
        if self.round_start {
            self.raise_inflight_longterm_slope();
        }
    }

    /// The probe's feedback and the long-term bound (the draft's
    /// AdaptLongTermModel); true when it changed the phase.
    fn adapt_long_term_model(&mut self, s: &Sample, app_limited: bool) -> bool {
        if self.ack_phase == AckPhase::ProbeStarting && self.round_start {
            self.ack_phase = AckPhase::ProbeFeedback;
        }
        if self.ack_phase == AckPhase::ProbeStopping && self.round_start {
            // The probe's samples are all in.
            self.is_bw_probe_sample = false;
            self.ack_phase = AckPhase::Init;
            // The bandwidth filter moves on a cycle, unless the cycle
            // that ends had no sample (as Linux: the estimate would fall
            // to nothing).
            if self.mode.probe_bw() && !app_limited && self.bw_hi[1] > 0.0 {
                self.bw_hi = [self.bw_hi[1], 0.0];
                self.max_bw = self.bw_hi[0];
            }
            // A probe stopped short for fear of loss, and none came:
            // probe again at once, all the way.
            if self.mode.probe_bw() && self.prev_probe_precautionary && !self.prev_probe_too_high {
                self.start_refill();
                return true;
            }
        }
        let too_high = s
            .tx_in_flight
            .is_some_and(|tx| s.lost_since as f64 > tx as f64 * LOSS_THRESH);
        if !too_high && self.inflight_longterm.is_finite() {
            if let Some(tx) = s.tx_in_flight
                && tx as f64 > self.inflight_longterm
            {
                self.inflight_longterm = tx as f64;
            }
            if self.mode == Mode::Up {
                self.probe_inflight_longterm_upward(s);
            }
        }
        false
    }

    fn time_to_probe(&mut self, now: Instant) -> bool {
        // The draft also probes after as many rounds as the window has
        // packets, when fewer than 63, to keep pace with Reno. A small
        // window behind a short queue then probes every few tenths of a
        // second, and each probe loses a few packets, and the voice of a
        // call on the same upload with them (see the module's comment):
        // here only a window of 63 packets or more probes by rounds, and a
        // smaller one waits the 2 to 3 s.
        let packets = (self.target_inflight() / f64::from(self.mtu)) as u64;
        let by_rounds = packets >= RENO_ROUNDS_MAX && self.rounds_since_probe_up >= RENO_ROUNDS_MAX;
        if now > self.cycle_stamp + self.bw_probe_wait || by_rounds {
            self.start_refill();
            return true;
        }
        false
    }

    fn time_to_go_down(&mut self, s: &Sample, rate: Option<f64>) -> bool {
        // The last probe lost too much: stop this one at its level.
        if self.prev_probe_too_high && s.inflight as f64 >= self.inflight_longterm {
            self.prev_probe_precautionary = true;
            return true;
        }
        if s.cwnd_limited && f64::from(self.cwnd) >= self.inflight_longterm {
            // Held by the bound, not by the path: the plateau says nothing.
            self.reset_full_bw();
            self.full_bw = rate.unwrap_or(0.0);
        } else if self.full_bw_now {
            return true;
        }
        false
    }

    fn update_probe_bw(&mut self, now: Instant, s: &Sample, rate: Option<f64>, app_limited: bool) {
        if !self.full_bw_reached {
            return;
        }
        if self.adapt_long_term_model(s, app_limited) {
            return;
        }
        match self.mode {
            Mode::Down => {
                if self.time_to_probe(now) {
                    return;
                }
                let cruise = s.inflight as f64 <= self.inflight_with_headroom()
                    && self
                        .inflight(1.0)
                        .is_some_and(|bdp| s.inflight as f64 <= bdp);
                if cruise {
                    self.start_cruise();
                }
            }
            Mode::Cruise => {
                self.time_to_probe(now);
            }
            Mode::Refill => {
                // After a round of refilling, probe.
                if self.round_start {
                    self.is_bw_probe_sample = true;
                    self.start_up(rate);
                }
            }
            Mode::Up => {
                if self.time_to_go_down(s, rate) {
                    self.prev_probe_too_high = false;
                    self.start_down(now);
                }
            }
            Mode::Startup | Mode::Drain | Mode::ProbeRtt => {}
        }
    }

    // ------------------------------------------------------------ ProbeRTT

    fn update_min_rtt(&mut self, now: Instant, s: &Sample) {
        self.probe_rtt_expired = now > self.probe_rtt_min_stamp + PROBE_RTT_INTERVAL;
        if let Some(rtt) = s.rtt
            && (self.probe_rtt_min_delay.is_none_or(|m| rtt < m) || self.probe_rtt_expired)
        {
            self.probe_rtt_min_delay = Some(rtt);
            self.probe_rtt_min_stamp = now;
        }
        let min_rtt_expired = now > self.min_rtt_stamp + MIN_RTT_FILTER;
        if let Some(delay) = self.probe_rtt_min_delay
            && (self.min_rtt.is_none_or(|m| delay < m) || min_rtt_expired)
        {
            self.min_rtt = Some(delay);
            self.min_rtt_stamp = self.probe_rtt_min_stamp;
        }
    }

    fn probe_rtt_cwnd(&self) -> f64 {
        self.bdp(PROBE_RTT_CWND_GAIN)
            .unwrap_or(0.0)
            .max(self.min_pipe())
    }

    fn check_probe_rtt(&mut self, now: Instant, s: &Sample) {
        if self.mode != Mode::ProbeRtt && self.probe_rtt_expired && self.min_rtt.is_some() {
            self.mode = Mode::ProbeRtt;
            self.pacing_gain = 1.0;
            self.cwnd_gain = PROBE_RTT_CWND_GAIN;
            self.save_cwnd();
            self.probe_rtt_done = None;
            self.ack_phase = AckPhase::ProbeStopping;
            self.start_round();
        }
        if self.mode != Mode::ProbeRtt {
            return;
        }
        match self.probe_rtt_done {
            None if s.inflight as f64 <= self.probe_rtt_cwnd() => {
                self.probe_rtt_done = Some(now + PROBE_RTT_DURATION);
                self.probe_rtt_round_done = false;
                self.start_round();
            }
            Some(done) => {
                if self.round_start {
                    self.probe_rtt_round_done = true;
                }
                if self.probe_rtt_round_done && now > done {
                    self.probe_rtt_min_stamp = now;
                    self.cwnd = self.cwnd.max(self.prior_cwnd);
                    self.bw_shortterm = f64::INFINITY;
                    self.inflight_shortterm = f64::INFINITY;
                    if self.full_bw_reached {
                        self.start_down(now);
                        self.start_cruise();
                    } else {
                        self.mode = Mode::Startup;
                        self.pacing_gain = STARTUP_PACING_GAIN;
                        self.cwnd_gain = DEFAULT_CWND_GAIN;
                    }
                }
            }
            None => {}
        }
    }

    fn save_cwnd(&mut self) {
        self.prior_cwnd = if !self.after_rto && self.mode != Mode::ProbeRtt {
            self.cwnd
        } else {
            self.prior_cwnd.max(self.cwnd)
        };
    }

    // ------------------------------------------------------------ control

    fn set_pacing_rate(&mut self) {
        if self.bw <= 0.0 {
            return;
        }
        let rate = self.pacing_gain * self.bw * (1.0 - PACING_MARGIN);
        // While Startup fills the pipe the rate only goes up.
        if self.full_bw_reached || rate > self.pacing_rate {
            self.pacing_rate = rate;
        }
    }

    fn set_cwnd(&mut self, s: &Sample) {
        if s.acked > 0 && self.after_rto {
            // Delivering again: back to where the window was.
            self.after_rto = false;
            self.cwnd = self.cwnd.max(self.prior_cwnd);
        }
        let acked = u32::try_from(s.acked).unwrap_or(u32::MAX);
        match self.inflight(self.cwnd_gain) {
            Some(max) if self.full_bw_reached => {
                self.cwnd = f64::from(self.cwnd.saturating_add(acked)).min(max) as u32;
            }
            Some(max) => {
                if f64::from(self.cwnd) < max || self.delivered < u64::from(self.initial_cwnd) {
                    self.cwnd = self.cwnd.saturating_add(acked);
                }
            }
            // No model yet: grow as slow start does.
            None if !self.full_bw_reached => self.cwnd = self.cwnd.saturating_add(acked),
            None => {}
        }
        let min_pipe = MIN_PIPE_PACKETS * self.mtu;
        self.cwnd = self.cwnd.max(min_pipe);
        if self.mode == Mode::ProbeRtt {
            self.cwnd = self.cwnd.min(self.probe_rtt_cwnd() as u32);
        }
        // The bounds from losses (the draft's BoundCwndForModel).
        let cap = if self.mode.probe_bw() && self.mode != Mode::Cruise {
            self.inflight_longterm
        } else if matches!(self.mode, Mode::ProbeRtt | Mode::Cruise) {
            self.inflight_with_headroom()
        } else {
            f64::INFINITY
        };
        let cap = cap.min(self.inflight_shortterm).max(f64::from(min_pipe));
        if cap.is_finite() {
            self.cwnd = self.cwnd.min(cap as u32);
        }
        if self.after_rto {
            self.cwnd = self.cwnd.min(min_pipe);
        }
    }
}

/// Below this rate (1.2 Mbit/s, as Linux's BBR) a burst is one packet.
const ONE_PACKET_BELOW: f64 = 150e3;

/// Lets data go at the pacing rate: credit grows with time, up to BURST's
/// worth (at least two packets, one on a slow path, as Linux's BBR), and
/// each packet spends its size. A short queue takes only so much at once:
/// four packets at a time, as before, overflowed a 2.5 KB queue by
/// themselves (10 Mbit/s with 2 ms of queue), and those losses set BBR's
/// bound at a quarter of the path's BDP; four are also most of the 5 KB a
/// 2 Mbit/s upload's 20 ms queue holds.
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
        let packets = if rate < ONE_PACKET_BELOW { 1 } else { 2 };
        let cap = (rate * BURST.as_secs_f64()).max(f64::from(packets * mtu));
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

    /// A path of `rate` bytes a second and `rtt` that holds at most `fits`
    /// bytes in flight (the line and its queue): a SACK a millisecond for
    /// `rounds` round trips. What goes past `fits` is lost, a chunk at a
    /// time, as RACK would find it a round trip later. Never short of data.
    struct Path {
        rate: f64,
        rtt: Duration,
        fits: f64,
        now: Instant,
        delivered: u64,
        lost_total: u64,
    }

    impl Path {
        fn new(rate: f64, rtt: Duration, fits: f64) -> Path {
            Path {
                rate,
                rtt,
                fits,
                now: Instant::now(),
                delivered: 0,
                lost_total: 0,
            }
        }

        fn bdp(&self) -> f64 {
            self.rate * self.rtt.as_secs_f64()
        }

        /// `rounds` round trips, a millisecond at a time: in flight is the
        /// window, or what the pacing puts out in a round trip if less;
        /// what does not fit in the line and its queue is lost (and found
        /// so at once); the rest arrives at the line's rate at most.
        fn run(&mut self, bbr: &mut Bbr, rounds: u32) {
            let step = Duration::from_millis(1);
            let rtt = self.rtt.as_secs_f64();
            let steps = (self.rtt.as_millis() as u32).max(1) * rounds;
            // Losses of the last round trip, a step at a time.
            let mut recent: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
            let mut owed = 0.0;
            for _ in 0..steps {
                self.now += step;
                let sending = f64::from(bbr.cwnd()).min(bbr.pacing_rate() * rtt);
                let inflight = sending.min(self.fits);
                let delivery = self.rate.min(inflight / rtt);
                let acked = (delivery * step.as_secs_f64()) as u64;
                self.delivered += acked;
                // The excess goes out at the pacing rate and is dropped.
                owed += (sending - self.fits).max(0.0) / rtt * step.as_secs_f64();
                let mut lost = 0;
                while owed >= f64::from(MTU) {
                    owed -= f64::from(MTU);
                    lost += u64::from(MTU);
                    self.lost_total += u64::from(MTU);
                    let since: u64 = recent.iter().sum::<u64>() + lost;
                    bbr.on_lost(
                        self.now,
                        Lost {
                            size: u64::from(MTU),
                            tx_in_flight: sending as u64,
                            lost_since: since,
                            app_limited: false,
                        },
                    );
                }
                recent.push_back(lost);
                if recent.len() > self.rtt.as_millis() as usize {
                    recent.pop_front();
                }
                // The chunk acknowledged now went out a round trip ago.
                let in_a_round_trip = (delivery * rtt) as u64;
                let queued = (inflight - self.bdp()).max(0.0) / self.rate;
                let s = Sample {
                    delivered: self.delivered,
                    acked,
                    lost,
                    prior_delivered: Some(self.delivered.saturating_sub(in_a_round_trip)),
                    rate: Some((delivery, false)),
                    sample_delivered: in_a_round_trip,
                    tx_in_flight: Some(inflight as u64),
                    lost_since: recent.iter().sum(),
                    rtt: Some(self.rtt + Duration::from_secs_f64(queued)),
                    inflight: inflight as u64,
                    cwnd_limited: f64::from(bbr.cwnd()) <= sending + f64::from(MTU),
                };
                bbr.on_sack(self.now, &s);
            }
        }
    }

    #[test]
    fn startup_finds_the_rate_and_cruises_there() {
        let mut path = Path::new(12.5e6, Duration::from_millis(40), f64::INFINITY);
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 40);
        assert!(bbr.full_bw_reached);
        assert!(bbr.mode().probe_bw(), "{:?}", bbr.mode());
        assert!((bbr.btl_bw() - path.rate).abs() / path.rate < 0.02);
        // About the rate (within ProbeBW's gains), and about two BDPs.
        let r = bbr.pacing_rate() / path.rate;
        assert!((0.85..=1.25).contains(&r), "{r}");
        let w = f64::from(bbr.cwnd()) / path.bdp();
        assert!((1.9..2.7).contains(&w), "{w}");
        assert!(bbr.inflight_longterm().is_none());
    }

    #[test]
    fn random_loss_under_the_threshold_keeps_the_rate() {
        let mut path = Path::new(12.5e6, Duration::from_millis(40), f64::INFINITY);
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 40);
        // One packet in about 400 lost every round, for 200 rounds (8 s,
        // probes included). Under the threshold the bound never comes;
        // each lossy round slows the short-term model toward what the round
        // delivered, until the next probe lifts it.
        let before = path.delivered;
        let mut slowest = f64::INFINITY;
        for _ in 0..200 {
            bbr.on_lost(
                path.now,
                Lost {
                    size: u64::from(MTU),
                    tx_in_flight: path.bdp() as u64,
                    lost_since: u64::from(MTU),
                    app_limited: false,
                },
            );
            path.run(&mut bbr, 1);
            slowest = slowest.min(bbr.pacing_rate());
        }
        assert!(bbr.inflight_longterm().is_none());
        let rate = (path.delivered - before) as f64 / (200.0 * path.rtt.as_secs_f64());
        eprintln!(
            "random loss: {:.2} of the line, slowest pacing {:.2}",
            rate / path.rate,
            slowest / path.rate
        );
        // Every round lossy, each round slows toward the last one's delivery
        // (paced 1 % under the estimate), and the next probe, 2 to 3 s
        // later, lifts it again. 0.87 of the line on average, here.
        assert!(rate >= 0.7 * path.rate, "{rate}");
        assert!(slowest >= 0.2 * path.rate, "{slowest}");
    }

    #[test]
    fn a_lossy_startup_ends_with_a_bound() {
        // The line and its queue hold 1.2 BDPs: Startup goes past that.
        let mut path = Path::new(6e6, Duration::from_millis(20), 0.0);
        path.fits = 1.2 * path.bdp();
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 12);
        assert!(bbr.full_bw_reached);
        assert_ne!(bbr.mode(), Mode::Startup);
        let bound = bbr.inflight_longterm().expect("a bound");
        assert!(bound <= 2.0 * path.fits, "{bound} {}", path.fits);
    }

    #[test]
    fn through_a_short_queue_it_stops_losing() {
        // 100 Mbit/s, 40 ms, a queue of 5 ms: 1.125 BDPs fit. BBRv1 kept 2.
        let mut path = Path::new(12.5e6, Duration::from_millis(40), 0.0);
        path.fits = 1.125 * path.bdp();
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 50);
        let lost_before = path.lost_total;
        let delivered_before = path.delivered;
        // Then 400 rounds (16 s): probes come every 2 to 3 s.
        path.run(&mut bbr, 400);
        let lost = (path.lost_total - lost_before) as f64;
        let delivered = (path.delivered - delivered_before) as f64;
        assert!(
            lost / (lost + delivered) < 0.01,
            "{}",
            lost / (lost + delivered)
        );
        // And it still goes near the line's rate.
        let rate = delivered / (400.0 * path.rtt.as_secs_f64());
        assert!(rate > 0.9 * path.rate, "{rate}");
        let bound = bbr.inflight_longterm().expect("a bound");
        assert!(bound < 1.5 * path.fits, "{bound}");
    }

    #[test]
    fn a_probe_that_loses_lowers_the_bound_and_goes_down() {
        let mut path = Path::new(12.5e6, Duration::from_millis(40), f64::INFINITY);
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 40);
        // Until it probes.
        let mut rounds = 0;
        while bbr.mode() != Mode::Up {
            path.run(&mut bbr, 1);
            rounds += 1;
            assert!(rounds < 200, "never probed");
        }
        let inflight = 1.1 * path.bdp();
        bbr.on_lost(
            path.now,
            Lost {
                size: u64::from(MTU),
                tx_in_flight: inflight as u64,
                lost_since: (0.05 * inflight) as u64,
                app_limited: false,
            },
        );
        assert_eq!(bbr.mode(), Mode::Down);
        // The losses before this chunk had passed the threshold already:
        // the bound is what was in flight before it.
        let bound = bbr.inflight_longterm().expect("a bound");
        let before = (inflight as u64 - u64::from(MTU)) as f64;
        assert!((bound - before).abs() <= 1.0, "{bound} {before}");
        // Cruising later, it keeps 15 % under the bound.
        let mut rounds = 0;
        while bbr.mode() != Mode::Cruise {
            path.run(&mut bbr, 1);
            rounds += 1;
            assert!(rounds < 50, "never cruised");
        }
        let bound = bbr.inflight_longterm().unwrap();
        assert!(f64::from(bbr.cwnd()) <= bound * (1.0 - HEADROOM) + 1.0);
    }

    #[test]
    fn a_cycle_without_samples_keeps_the_bandwidth() {
        let mut path = Path::new(12.5e6, Duration::from_millis(40), f64::INFINITY);
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 40);
        assert!(bbr.mode().probe_bw(), "{:?}", bbr.mode());
        let bw = bbr.btl_bw();
        // The end of a probe's feedback comes on a SACK without a rate
        // sample, after a cycle that had none either.
        bbr.ack_phase = AckPhase::ProbeStopping;
        bbr.bw_hi = [bw, 0.0];
        bbr.next_round_delivered = path.delivered;
        path.now += Duration::from_millis(1);
        let s = Sample {
            delivered: path.delivered + 1000,
            acked: 1000,
            prior_delivered: Some(path.delivered),
            inflight: path.bdp() as u64,
            ..Default::default()
        };
        bbr.on_sack(path.now, &s);
        assert!((bbr.btl_bw() - bw).abs() < 1.0, "{} {bw}", bbr.btl_bw());
        assert!(bbr.pacing_rate() > 0.5 * bw);
    }

    #[test]
    fn up_raises_the_bound_while_nothing_is_lost() {
        let mut path = Path::new(12.5e6, Duration::from_millis(40), f64::INFINITY);
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 40);
        // A bound well under the path, as after a loss that has passed.
        bbr.inflight_longterm = 0.5 * path.bdp();
        let mut rounds = 0;
        while bbr.mode() != Mode::Up {
            path.run(&mut bbr, 1);
            rounds += 1;
            assert!(rounds < 200, "never probed");
        }
        let start = bbr.inflight_longterm().unwrap();
        path.run(&mut bbr, 6);
        let after = bbr.inflight_longterm().unwrap();
        // 1 + 2 + 4 + ... packets a round.
        assert!(after >= start + 15.0 * f64::from(MTU), "{start} {after}");
    }

    #[test]
    fn probe_rtt_comes_every_five_seconds_and_goes() {
        let mut path = Path::new(5e6, Duration::from_millis(50), f64::INFINITY);
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        let mut seen = false;
        // Over five seconds at the same round trip, and then some.
        for _ in 0..160 {
            path.run(&mut bbr, 1);
            seen |= bbr.mode() == Mode::ProbeRtt;
        }
        assert!(seen);
        assert!(bbr.mode().probe_bw(), "{:?}", bbr.mode());
    }

    #[test]
    fn a_timeout_shrinks_the_window_until_delivery() {
        let mut path = Path::new(12.5e6, Duration::from_millis(40), f64::INFINITY);
        let mut bbr = Bbr::new(path.now, MTU, 4380);
        path.run(&mut bbr, 40);
        let rate = bbr.btl_bw();
        bbr.on_rto();
        assert_eq!(bbr.cwnd(), 4 * MTU);
        path.run(&mut bbr, 2);
        // Back to what the last round delivered (the timeout counts as a
        // loss for the short-term model), enough to keep the line full; the
        // path's rate as it was.
        assert!(f64::from(bbr.cwnd()) >= 0.9 * path.bdp(), "{}", bbr.cwnd());
        assert!((bbr.btl_bw() - rate).abs() / rate < 0.02);
    }

    #[test]
    fn the_pacer_spreads_a_burst() {
        let t = Instant::now();
        let mut p = Pacer::default();
        let rate = 1e6; // a byte a microsecond
        // The first wake-up may send BURST's worth (4 KB at this rate).
        let mut sent = 0;
        while p.admit(t, rate, MTU, 1000).is_ok() {
            sent += 1000;
        }
        assert!((4000..=5000).contains(&sent), "{sent}");
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

    #[test]
    fn on_a_slow_path_a_burst_is_a_packet_or_two() {
        let t = Instant::now();
        let full = MTU as usize;
        // 2 Mbit/s: two packets at once, then the rest spread out.
        let mut p = Pacer::default();
        let mut sent = 0;
        while p.admit(t, 250e3, MTU, full).is_ok() {
            sent += 1;
        }
        assert_eq!(sent, 2);
        // 1 Mbit/s: one.
        let mut p = Pacer::default();
        let mut sent = 0;
        while p.admit(t, 125e3, MTU, full).is_ok() {
            sent += 1;
        }
        assert_eq!(sent, 1);
    }
}
