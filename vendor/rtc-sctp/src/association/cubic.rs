//! CUBIC congestion control for the sending side (RFC 9438), to set beside
//! Reno and BBR (Babu Tela).
//!
//! CUBIC is TCP's default on Windows, Linux and macOS, and most QUIC stacks
//! offer it. Like Reno it reads a loss as congestion, but a loss takes 30 %
//! off the window instead of half, and the window grows back along a cubic
//! curve of the time since the loss: fast while far below the window the
//! loss came at, slowly near it, faster again past it. While that curve
//! would grow slower than Reno does (short paths, small windows), the window
//! follows Reno's growth instead (§4.3), so on such a path it behaves as
//! Reno with a gentler cut.
//!
//! Slow start and fast recovery stay the association's (Reno's); this only
//! decides the window in congestion avoidance, and how much a loss or a
//! timeout takes off.

use std::time::{Duration, Instant};

/// RFC 9438 §5: the curve's scale, in segments per second cubed.
const C: f64 = 0.4;
/// What a loss leaves of the window.
const BETA: f64 = 0.7;
/// Reno-friendly growth, in segments per round trip, while below the window
/// of the last loss (§4.3): 3 (1 − β) / (1 + β).
const ALPHA: f64 = 3.0 * (1.0 - BETA) / (1.0 + BETA);
/// No SACK that grew the window for this long (or two round trips, if
/// longer): the sender was short of data in between.
const IDLE_AFTER: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Default)]
pub(crate) struct Cubic {
    /// W_max: the window (in segments) the last loss came at, where the
    /// curve flattens.
    w_max: f64,
    /// cwnd_prior: the window when the last loss came, in segments.
    prior: f64,
    /// The current stage of congestion avoidance, from its first SACK.
    epoch: Option<Epoch>,
    /// When the window last grew (a SACK while it was in use).
    last_ack: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Epoch {
    start: Instant,
    /// K: the seconds the curve takes to come back to W_max.
    k: f64,
    /// W_est: where Reno's growth would have the window, in segments.
    w_est: f64,
}

impl Cubic {
    /// A loss the SACKs showed (entering fast recovery), at window `cwnd`
    /// (bytes): the new slow start threshold, bytes, which is also the new
    /// window.
    pub(crate) fn on_loss(&mut self, cwnd: u32, mtu: u32) -> u32 {
        let w = f64::from(cwnd) / f64::from(mtu);
        // Fast convergence (§4.7): a loss below the last plateau means
        // another flow is taking a share, and the curve flattens lower to
        // leave it room.
        self.w_max = if w < self.w_max {
            w * (1.0 + BETA) / 2.0
        } else {
            w
        };
        self.prior = w;
        self.epoch = None;
        ((f64::from(cwnd) * BETA) as u32).max(4 * mtu)
    }

    /// A retransmission timeout at window `cwnd` (bytes): the new slow
    /// start threshold, bytes (the window goes to one packet, as Reno's).
    /// The curve of the next congestion avoidance starts flat where that
    /// starts (§4.8).
    pub(crate) fn on_timeout(&mut self, cwnd: u32, mtu: u32) -> u32 {
        self.prior = f64::from(cwnd) / f64::from(mtu);
        self.w_max = 0.0;
        self.epoch = None;
        ((f64::from(cwnd) * BETA) as u32).max(4 * mtu)
    }

    /// Congestion avoidance: the window (bytes) after a SACK newly
    /// acknowledged `acked` bytes, `cwnd` being the window now and `rtt`
    /// the smoothed round trip.
    pub(crate) fn on_ack(
        &mut self,
        cwnd: u32,
        acked: u32,
        mtu: u32,
        rtt: Duration,
        now: Instant,
    ) -> u32 {
        let seg = f64::from(mtu.max(1));
        let w = (f64::from(cwnd) / seg).max(1.0);
        // §5.8: the time spent short of data (quiet, or sending less than
        // the window allows, when the association does not call this) is
        // not on the curve, or the window would leap after a quiet spell.
        // A call's data channel is quiet most of the time.
        if let (Some(last), Some(epoch)) = (self.last_ack, self.epoch.as_mut()) {
            let gap = now.saturating_duration_since(last);
            if gap > IDLE_AFTER.max(2 * rtt) {
                epoch.start = (epoch.start + gap).min(now);
            }
        }
        self.last_ack = Some(now);
        if self.epoch.is_none() {
            // A stage that starts above the plateau (the first one, or
            // after a timeout) has the plateau where it starts: K is 0.
            self.w_max = self.w_max.max(w);
            self.epoch = Some(Epoch {
                start: now,
                k: ((self.w_max - w) / C).cbrt(),
                w_est: w,
            });
        }
        let (w_max, prior) = (self.w_max, self.prior);
        let Some(epoch) = self.epoch.as_mut() else {
            return cwnd;
        };
        let k = epoch.k;
        let curve = |t: f64| C * (t - k).powi(3) + w_max;
        let t = now.saturating_duration_since(epoch.start).as_secs_f64();
        let segments = f64::from(acked) / seg;
        // §4.3: Reno's own growth, a segment a round trip once past the
        // window of the last loss.
        let alpha = if epoch.w_est >= prior { 1.0 } else { ALPHA };
        epoch.w_est += alpha * segments / w;
        let next = if curve(t) < epoch.w_est {
            epoch.w_est
        } else {
            // §4.4, §4.5: toward where the curve will be a round trip from
            // now, by at most half the window.
            let target = curve(t + rtt.as_secs_f64()).clamp(w, 1.5 * w);
            w + (target - w) / w * segments
        };
        ((next * seg) as u32).max(cwnd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: u32 = 1200;

    fn segs(bytes: u32) -> f64 {
        f64::from(bytes) / f64::from(MTU)
    }

    /// A round trip at a time, each acknowledging the whole window, from
    /// `cwnd` for `for_s` seconds: the window at the end.
    fn grow(c: &mut Cubic, mut cwnd: u32, rtt: Duration, start: Instant, for_s: f64) -> u32 {
        let mut now = start;
        while now.duration_since(start).as_secs_f64() < for_s {
            cwnd = c.on_ack(cwnd, cwnd, MTU, rtt, now);
            now += rtt;
        }
        cwnd
    }

    #[test]
    fn a_loss_keeps_seventy_percent() {
        let mut c = Cubic::default();
        let ssthresh = c.on_loss(100 * MTU, MTU);
        assert!((segs(ssthresh) - 70.0).abs() < 0.01, "{}", segs(ssthresh));
        assert_eq!(c.w_max, 100.0);
        // Never below four packets, as Reno's.
        assert_eq!(c.on_loss(3 * MTU, MTU), 4 * MTU);
    }

    #[test]
    fn the_curve_comes_back_to_the_loss_after_k_then_climbs() {
        // A long round trip, where the curve is above Reno's growth.
        let rtt = Duration::from_millis(100);
        let mut c = Cubic::default();
        let cwnd = c.on_loss(100 * MTU, MTU);
        let start = Instant::now();
        // K = cbrt(100 × 0.3 / 0.4) = 4.22 s.
        let early = grow(&mut c.clone(), cwnd, rtt, start, 1.0);
        let at_k = grow(&mut c.clone(), cwnd, rtt, start, 4.2);
        let late = grow(&mut c, cwnd, rtt, start, 7.0);
        // Fast at first (concave), flat near the plateau, then past it.
        assert!(segs(early) > 85.0, "{}", segs(early));
        assert!((97.0..103.0).contains(&segs(at_k)), "{}", segs(at_k));
        assert!(segs(late) > 105.0, "{}", segs(late));
    }

    #[test]
    fn on_a_short_path_it_grows_as_reno_does() {
        // 10 ms: in a second Reno adds a hundred packets; the curve, a
        // fraction of that.
        let rtt = Duration::from_millis(10);
        let mut c = Cubic::default();
        let cwnd = c.on_loss(20 * MTU, MTU);
        let after = grow(&mut c, cwnd, rtt, Instant::now(), 1.0);
        assert!((95.0..115.0).contains(&segs(after)), "{}", segs(after));
    }

    #[test]
    fn a_loss_below_the_plateau_lowers_it() {
        let mut c = Cubic::default();
        c.on_loss(100 * MTU, MTU);
        c.on_loss(80 * MTU, MTU);
        assert!((c.w_max - 68.0).abs() < 1e-9, "{}", c.w_max);
    }

    #[test]
    fn after_a_timeout_the_curve_starts_where_the_window_is() {
        let rtt = Duration::from_millis(100);
        let mut c = Cubic::default();
        c.on_loss(100 * MTU, MTU);
        let ssthresh = c.on_timeout(100 * MTU, MTU);
        assert!((segs(ssthresh) - 70.0).abs() < 0.01, "{}", segs(ssthresh));
        // Slow start brought it back to 50 packets: the curve is flat
        // there (K = 0) and only climbs.
        let start = Instant::now();
        let first = c.on_ack(50 * MTU, 50 * MTU, MTU, rtt, start);
        assert_eq!(c.w_max, 50.0);
        assert!((50.0..51.5).contains(&segs(first)), "{}", segs(first));
        let later = grow(&mut c, first, rtt, start + rtt, 3.0);
        assert!(segs(later) > 55.0, "{}", segs(later));
    }

    #[test]
    fn a_quiet_spell_does_not_count_on_the_curve() {
        let rtt = Duration::from_millis(100);
        let start = Instant::now();
        let mut busy = Cubic::default();
        let cwnd = busy.on_loss(100 * MTU, MTU);
        let mut quiet = busy.clone();
        // Two seconds in use; then one of them stops for a minute.
        let at_2s = grow(&mut busy, cwnd, rtt, start, 2.0);
        assert_eq!(grow(&mut quiet, cwnd, rtt, start, 2.0), at_2s);
        let ticks = 20u32;
        let next_busy = busy.on_ack(at_2s, at_2s, MTU, rtt, start + rtt * ticks);
        let next_quiet = quiet.on_ack(
            at_2s,
            at_2s,
            MTU,
            rtt,
            start + rtt * ticks + Duration::from_secs(60),
        );
        // A minute more on the curve would be the most it grows (half the
        // window); without it, it grows as the busy one does.
        assert!(
            segs(next_quiet) < 1.05 * segs(at_2s),
            "{}",
            segs(next_quiet)
        );
        assert!((segs(next_quiet) - segs(next_busy)).abs() < 1.0);
    }

    #[test]
    fn it_never_shrinks_the_window_while_growing() {
        let rtt = Duration::from_millis(40);
        let mut c = Cubic::default();
        let mut cwnd = c.on_loss(300 * MTU, MTU);
        let mut now = Instant::now();
        for _ in 0..500 {
            let next = c.on_ack(cwnd, 4 * MTU, MTU, rtt, now);
            assert!(next >= cwnd);
            cwnd = next;
            now += Duration::from_millis(5);
        }
    }
}
