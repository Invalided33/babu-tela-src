//! The decisions that keep a call light however many people are in it.
//!
//! Calls are peer to peer: each pair of people has a connection of its own
//! and no server touches the media, so what each person sends grows with
//! the size of the call. These rules hold that in check:
//!
//! - **Voice**: Opus with DTX (nothing goes out while someone is quiet), a
//!   bitrate that falls as the call grows (24 kb/s is still full band) and
//!   longer packets in big calls, since every packet pays about 50 bytes of
//!   headers however little it carries.
//! - **Video in two copies**, made by the sender itself (simulcast): a small
//!   one for tiles and one at the chosen quality. Each person says, over
//!   the connection itself, which copy they want of whom: none of someone
//!   they are not looking at (another channel open, a tile scrolled away,
//!   past the nine shown), the small one for a tile, the big one for an
//!   enlarged tile.
//! - **A budget** for the video sent to everyone together, which grows a
//!   little at a time while nobody complains and falls when most of them
//!   report loss or a growing delay (AIMD, like TCP and WebRTC's own
//!   congestion control).
//! - **Per person**: someone on a bad connection gets less (the small copy,
//!   or none) without dragging the others down, and tries for more again
//!   after a quiet while, waiting longer after each failure (the "join
//!   experiments" of receiver-driven layered multicast).
//!
//! Everything here is plain logic over numbers, so it is tested on its own;
//! rtc.rs feeds it the reports and applies the answers.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::Quality;
use crate::model::Video;

// ---------------------------------------------------------------- voice

/// How the voice goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoicePlan {
    /// Opus bitrate, in bits per second.
    pub bitrate: i32,
    /// How much sound each packet carries, in milliseconds.
    pub frame_ms: u32,
}

impl Default for VoicePlan {
    fn default() -> VoicePlan {
        voice_plan(1)
    }
}

/// For a call with `others` other people, each of whom gets a copy of
/// every packet. At 50 packets a second the headers alone cost about
/// 22 kb/s per copy, so big calls send fewer, longer packets. Small ones
/// send 10 ms packets: measured on one PC, mouth to ear went from 121 to
/// 80-88 ms (half the packet to wait for, and half the queue to hold),
/// for 22 kb/s more of headers per person.
pub fn voice_plan(others: usize) -> VoicePlan {
    let (bitrate, frame_ms) = match others {
        0..=3 => (48_000, 10),
        4..=7 => (32_000, 20),
        8..=15 => (24_000, 40),
        _ => (20_000, 60),
    };
    VoicePlan { bitrate, frame_ms }
}

// ---------------------------------------------------------------- video

/// Which copy of someone's video: none, the small one, or the big one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    #[default]
    Off,
    Low,
    High,
}

impl Layer {
    pub fn up(self) -> Layer {
        match self {
            Layer::Off => Layer::Low,
            _ => Layer::High,
        }
    }

    pub fn down(self) -> Layer {
        match self {
            Layer::High => Layer::Low,
            _ => Layer::Off,
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Layer::Off => "off",
            Layer::Low => "low",
            Layer::High => "high",
        }
    }

    pub fn from_wire(s: &str) -> Option<Layer> {
        match s {
            "off" => Some(Layer::Off),
            "low" => Some(Layer::Low),
            "high" => Some(Layer::High),
            _ => None,
        }
    }

    /// The encoder that makes this copy: 0 for the small one, 1 for the big.
    pub fn slot(self) -> Option<usize> {
        match self {
            Layer::Off => None,
            Layer::Low => Some(0),
            Layer::High => Some(1),
        }
    }

    pub const COPIES: [Layer; 2] = [Layer::Low, Layer::High];
}

impl serde::Serialize for Layer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.wire())
    }
}

/// What my video is encoded in. H.264 is what every copy of the app reads
/// and what openh264 makes (the camera, and screens without an encoder on
/// the card); AV1, from the newer cards' encoders, takes about a third
/// fewer bytes for the same picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Codec {
    #[default]
    H264,
    Av1,
}

impl Codec {
    pub fn wire(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Av1 => "av1",
        }
    }

    pub fn from_wire(s: &str) -> Option<Codec> {
        match s {
            "h264" => Some(Codec::H264),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }
}

impl serde::Serialize for Codec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.wire())
    }
}

/// AV1 when this PC encodes it and everyone in the call reads it, H.264
/// otherwise: each copy is one stream that goes to all of them. `peers`
/// holds whether each one reads AV1, None while they have not said (taken
/// as yes: an older copy of the app, which never says, is found out by
/// the first thing it does say, see rtc.rs).
pub fn pick_codec(encodes_av1: bool, peers: impl IntoIterator<Item = Option<bool>>) -> Codec {
    if encodes_av1 && peers.into_iter().all(|av1| av1 != Some(false)) {
        Codec::Av1
    } else {
        Codec::H264
    }
}

/// One copy of a video: the largest picture, frames per second, and the
/// bitrate (kb/s) below which it is not worth sending and above which it
/// gains nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub min_kbps: u32,
    pub max_kbps: u32,
}

/// The two copies of one source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copies {
    pub low: Spec,
    pub high: Spec,
}

/// The copies of a camera or a screen at the chosen quality. A screen's
/// small copy is larger and slower (text needs pixels more than motion).
pub fn copies(kind: Video, q: Quality) -> Copies {
    match kind {
        Video::Camera => Copies {
            low: Spec {
                width: 480,
                height: 270,
                fps: 15,
                min_kbps: 120,
                max_kbps: 300,
            },
            high: Spec {
                width: q.width,
                height: q.height,
                fps: q.fps,
                min_kbps: 400,
                max_kbps: q.camera.max(400),
            },
        },
        Video::Screen => Copies {
            low: Spec {
                width: 640,
                height: 360,
                fps: 8,
                min_kbps: 150,
                max_kbps: 400,
            },
            high: Spec {
                width: q.width,
                height: q.height,
                fps: q.fps,
                min_kbps: 700,
                max_kbps: q.screen.max(700),
            },
        },
    }
}

/// What everyone gets of my video, and the bitrate of each copy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allocation {
    pub layers: Vec<(i64, Layer)>,
    pub low_kbps: u32,
    pub high_kbps: u32,
    /// The budget is what holds the video back (someone gets less than
    /// they can take, or a copy is short of its best): worth growing.
    pub short: bool,
}

impl Allocation {
    pub fn layer(&self, uid: i64) -> Layer {
        self.layers
            .iter()
            .find(|(u, _)| *u == uid)
            .map_or(Layer::Off, |(_, l)| *l)
    }

    /// The video this sends, in kb/s.
    pub fn kbps(&self) -> u32 {
        self.layers
            .iter()
            .map(|(_, l)| match l {
                Layer::Off => 0,
                Layer::Low => self.low_kbps,
                Layer::High => self.high_kbps,
            })
            .sum()
    }
}

/// Shares `budget` (kb/s of video for everyone together) among the people
/// who asked for my video. `asks` holds each one with the most they can
/// take (what they asked for, held to what their connection carries), most
/// important first. Everyone gets the small copy before anyone gets the
/// big one; what is left raises the bitrates, small copies first.
pub fn allocate(asks: &[(i64, Layer)], budget: u32, c: &Copies) -> Allocation {
    let mut out = Allocation::default();
    let mut used = 0u32;
    for &(uid, ask) in asks {
        let got = if ask >= Layer::Low && used + c.low.min_kbps <= budget {
            used += c.low.min_kbps;
            Layer::Low
        } else {
            out.short |= ask > Layer::Off;
            Layer::Off
        };
        out.layers.push((uid, got));
    }
    let step = c.high.min_kbps.saturating_sub(c.low.min_kbps);
    for (i, &(_, ask)) in asks.iter().enumerate() {
        if ask == Layer::High && out.layers[i].1 == Layer::Low {
            if used + step <= budget {
                used += step;
                out.layers[i].1 = Layer::High;
            } else {
                out.short = true;
            }
        }
    }
    let count = |layer| out.layers.iter().filter(|(_, l)| *l == layer).count() as u32;
    let (n_low, n_high) = (count(Layer::Low), count(Layer::High));
    let mut left = budget.saturating_sub(used);
    out.low_kbps = c.low.max_kbps;
    if let Some(each) = left.checked_div(n_low) {
        let extra = each.min(c.low.max_kbps.saturating_sub(c.low.min_kbps));
        out.low_kbps = c.low.min_kbps + extra;
        left -= extra * n_low;
        out.short |= out.low_kbps < c.low.max_kbps;
    }
    out.high_kbps = c.high.max_kbps;
    if let Some(each) = left.checked_div(n_high) {
        let extra = each.min(c.high.max_kbps.saturating_sub(c.high.min_kbps));
        out.high_kbps = c.high.min_kbps + extra;
        out.short |= out.high_kbps < c.high.max_kbps;
    }
    out
}

// ---------------------------------------------------------------- the network

/// Loss above this is congestion (where WebRTC's loss-based control backs
/// off); above `STRAINED_LOSS`, at least nothing grows.
const CONGESTED_LOSS: f64 = 0.10;
const STRAINED_LOSS: f64 = 0.03;

/// A round trip this much longer than the shortest seen (seconds) means my
/// packets are queueing somewhere on the way.
const QUEUE_CONGESTED: f64 = 0.20;
const QUEUE_STRAINED: f64 = 0.08;

/// The first experiment waits this long without trouble; each failure
/// doubles the wait, up to the last.
const FIRST_WAIT: Duration = Duration::from_secs(4);
const MAX_WAIT: Duration = Duration::from_secs(64);

/// An experiment that lasts this long without congestion has worked.
const TRIAL: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Good,
    Strained,
    Congested,
}

/// What the network lets me send one person, learned from their reports.
#[derive(Clone, Debug)]
pub struct Link {
    /// The most this connection carries of my video.
    pub cap: Layer,
    min_rtt: Option<f64>,
    quiet_since: Instant,
    wait: Duration,
    trial: Option<Instant>,
}

impl Link {
    /// A new connection may take everything; trouble teaches it otherwise.
    pub fn new(now: Instant) -> Link {
        Link {
            cap: Layer::High,
            min_rtt: None,
            quiet_since: now,
            wait: FIRST_WAIT,
            trial: None,
        }
    }

    /// How one report reads: `loss` is the fraction of my packets they
    /// lost since their previous report, `rtt` the round trip in seconds.
    pub fn health(&mut self, loss: Option<f64>, rtt: Option<f64>) -> Health {
        let queue = rtt.map_or(0.0, |rtt| {
            // The shortest round trip is the path without queues. It creeps
            // up a millisecond a report, so a new, longer path is learned.
            let base = self.min_rtt.map_or(rtt, |m| (m + 0.001).min(rtt));
            self.min_rtt = Some(base);
            rtt - base
        });
        let loss = loss.unwrap_or(0.0);
        if loss > CONGESTED_LOSS || queue > QUEUE_CONGESTED {
            Health::Congested
        } else if loss > STRAINED_LOSS || queue > QUEUE_STRAINED {
            Health::Strained
        } else {
            Health::Good
        }
    }

    /// Acts on one report. `shared` means most people receiving my video
    /// are congested at once, so the trouble is my own uplink, which the
    /// budget answers for; this link steps down only for trouble of its
    /// own, or when an experiment of its own fails.
    pub fn update(&mut self, now: Instant, health: Health, shared: bool, ask: Layer) {
        if health == Health::Congested {
            self.quiet_since = now;
            if self.trial.take().is_some() {
                // The step up was too much: back down, and wait longer
                // before trying again.
                self.cap = self.cap.down();
                self.wait = (self.wait * 2).min(MAX_WAIT);
            } else if !shared {
                self.cap = self.cap.down();
            }
            return;
        }
        if let Some(at) = self.trial
            && now.saturating_duration_since(at) >= TRIAL
        {
            self.trial = None;
            self.wait = (self.wait * 3 / 4).max(FIRST_WAIT);
        }
        if health == Health::Strained {
            self.quiet_since = now;
            return;
        }
        if self.trial.is_none()
            && self.cap < ask
            && now.saturating_duration_since(self.quiet_since) >= self.wait
        {
            self.cap = self.cap.up();
            self.trial = Some(now);
            self.quiet_since = now;
        }
    }
}

/// The video budget starts at what most connections upload, and is kept
/// between these (kb/s).
pub const START_BUDGET: u32 = 3_000;
const MIN_BUDGET: u32 = 150;
const MAX_BUDGET: u32 = 100_000;

/// After a cut, the next waits this long (the reports that prompted it
/// describe the past), and growth longer still.
const CUT_HOLD: Duration = Duration::from_millis(1_500);
const GROW_HOLD: Duration = Duration::from_secs(3);

/// The kb/s of video for everyone together. The uplink's capacity is not
/// known, so this learns it: +6 % a second (at least 100 kb/s) while the
/// budget is what holds the video back and nobody complains, and down to
/// 85 % of what actually went out when most receivers report congestion.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub kbps: u32,
    last_cut: Option<Instant>,
}

impl Default for Budget {
    fn default() -> Budget {
        Budget {
            kbps: START_BUDGET,
            last_cut: None,
        }
    }
}

impl Budget {
    /// Once a second. `sent_kbps`: the video that went out over the last
    /// second; `congested` of the `receivers` of my video reported trouble;
    /// `short` is the allocation's.
    pub fn update(
        &mut self,
        now: Instant,
        sent_kbps: u32,
        congested: usize,
        receivers: usize,
        short: bool,
    ) {
        let since_cut = self.last_cut.map(|at| now.saturating_duration_since(at));
        if receivers > 0 && congested * 2 >= receivers {
            if since_cut.is_none_or(|d| d >= CUT_HOLD) {
                let cut = (f64::from(sent_kbps) * 0.85) as u32;
                self.kbps = self.kbps.min(cut).max(MIN_BUDGET);
                self.last_cut = Some(now);
            }
        } else if congested == 0 && short && since_cut.is_none_or(|d| d >= GROW_HOLD) {
            self.kbps = (self.kbps + (self.kbps / 16).max(100)).min(MAX_BUDGET);
        }
    }
}

/// Whether most receivers are congested at once (my own uplink, then).
pub fn shared_congestion(congested: usize, receivers: usize) -> bool {
    receivers > 0 && congested * 2 >= receivers
}

// ---------------------------------------------------------------- what to show

/// At most this many videos are received at once (the rest show their
/// picture): past nine, tiles are too small to tell faces apart anyway,
/// and each one costs a decoder here and an upload there.
pub const MAX_VIDEOS: usize = 9;

/// Someone whose video could be shown.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub uid: i64,
    pub zoomed: bool,
    pub screen: bool,
    /// When they last spoke.
    pub spoke: Option<Instant>,
}

/// The videos to receive, at most `n` (last-N): an enlarged one first, then
/// shared screens, then whoever spoke last. Someone shown already keeps
/// their place over someone who has not spoken since, so tiles do not keep
/// trading places.
pub fn pick_videos(cands: &[Candidate], showing: &HashSet<i64>, n: usize) -> HashSet<i64> {
    let mut sorted: Vec<&Candidate> = cands.iter().collect();
    sorted.sort_by(|a, b| {
        b.zoomed
            .cmp(&a.zoomed)
            .then(b.screen.cmp(&a.screen))
            .then(b.spoke.cmp(&a.spoke))
            .then(showing.contains(&b.uid).cmp(&showing.contains(&a.uid)))
            .then(a.uid.cmp(&b.uid))
    });
    sorted.into_iter().take(n).map(|c| c.uid).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Copies = Copies {
        low: Spec {
            width: 640,
            height: 360,
            fps: 8,
            min_kbps: 150,
            max_kbps: 400,
        },
        high: Spec {
            width: 1920,
            height: 1080,
            fps: 30,
            min_kbps: 700,
            max_kbps: 4500,
        },
    };

    #[test]
    fn the_voice_gets_lighter_as_the_call_grows() {
        assert_eq!(
            voice_plan(1),
            VoicePlan {
                bitrate: 48_000,
                frame_ms: 10
            }
        );
        assert_eq!(voice_plan(5).bitrate, 32_000);
        assert_eq!(
            voice_plan(10),
            VoicePlan {
                bitrate: 24_000,
                frame_ms: 40
            }
        );
        assert_eq!(voice_plan(30).frame_ms, 60);
        // What one person uploads while speaking, headers included (about
        // 56 bytes a packet), stays modest even for 30 people.
        let upload = |others: usize| {
            let p = voice_plan(others);
            let packets = 1000 / p.frame_ms as usize;
            others * (p.bitrate as usize + packets * 56 * 8) / 1000
        };
        assert!(upload(30) < 900, "{} kb/s", upload(30));
        assert!(upload(3) < 290, "{} kb/s", upload(3));
    }

    #[test]
    fn layers_step_and_travel() {
        assert_eq!(Layer::Off.up(), Layer::Low);
        assert_eq!(Layer::High.up(), Layer::High);
        assert_eq!(Layer::High.down(), Layer::Low);
        assert_eq!(Layer::Off.down(), Layer::Off);
        for l in [Layer::Off, Layer::Low, Layer::High] {
            assert_eq!(Layer::from_wire(l.wire()), Some(l));
        }
        assert_eq!(Layer::from_wire("4k"), None);
        assert!(Layer::Off < Layer::Low && Layer::Low < Layer::High);
        assert_eq!(serde_json::to_value(Layer::High).unwrap(), "high");
    }

    #[test]
    fn plenty_of_budget_gives_everyone_what_they_ask() {
        let asks = [(1, Layer::High), (2, Layer::Low), (3, Layer::Off)];
        let a = allocate(&asks, 50_000, &SCREEN);
        assert_eq!(a.layers, asks);
        assert_eq!((a.low_kbps, a.high_kbps), (400, 4500));
        assert!(!a.short);
        assert_eq!(a.kbps(), 4900);
    }

    #[test]
    fn a_tight_budget_serves_small_copies_first() {
        let asks = [(1, Layer::High), (2, Layer::High), (3, Layer::Low)];
        // 3 small copies (450) and one big (+550) fit in 1100; a second big
        // does not.
        let a = allocate(&asks, 1_100, &SCREEN);
        assert_eq!(a.layer(1), Layer::High);
        assert_eq!(a.layer(2), Layer::Low);
        assert_eq!(a.layer(3), Layer::Low);
        assert!(a.short);
        assert!(a.kbps() <= 1_100, "{}", a.kbps());
        // Below one small copy, nobody gets video, and that is short too.
        let a = allocate(&asks, 100, &SCREEN);
        assert!(a.layers.iter().all(|(_, l)| *l == Layer::Off));
        assert!(a.short);
        // Many people: as many small copies as fit, in order.
        let crowd: Vec<(i64, Layer)> = (0..30).map(|i| (i, Layer::Low)).collect();
        let a = allocate(&crowd, 3_000, &SCREEN);
        assert_eq!(
            a.layers.iter().filter(|(_, l)| *l == Layer::Low).count(),
            20
        );
        assert_eq!(a.layer(19), Layer::Low);
        assert_eq!(a.layer(20), Layer::Off);
        assert!(a.kbps() <= 3_000);
    }

    #[test]
    fn what_is_left_raises_the_bitrates() {
        let asks = [(1, Layer::High), (2, Layer::Low)];
        let a = allocate(&asks, 2_000, &SCREEN);
        assert_eq!(a.layers, asks);
        // The small copy gets its best first, the big one the rest.
        assert_eq!(a.low_kbps, 400);
        assert_eq!(a.high_kbps, 1_600);
        assert_eq!(a.kbps(), 2_000);
        assert!(a.short, "a big copy below its best wants more budget");
    }

    #[test]
    fn a_bad_connection_steps_down_on_its_own() {
        let t0 = Instant::now();
        let mut link = Link::new(t0);
        assert_eq!(link.health(Some(0.0), Some(0.050)), Health::Good);
        assert_eq!(link.health(Some(0.25), Some(0.050)), Health::Congested);
        assert_eq!(link.health(Some(0.05), Some(0.050)), Health::Strained);
        // Queueing: the round trip grows well past the shortest seen.
        assert_eq!(link.health(Some(0.0), Some(0.400)), Health::Congested);
        assert_eq!(link.health(Some(0.0), Some(0.150)), Health::Strained);

        link.update(t0, Health::Congested, false, Layer::High);
        assert_eq!(link.cap, Layer::Low);
        // My own uplink being the trouble is the budget's business.
        link.update(t0, Health::Congested, true, Layer::High);
        assert_eq!(link.cap, Layer::Low);
        link.update(t0, Health::Congested, false, Layer::High);
        assert_eq!(link.cap, Layer::Off);
    }

    #[test]
    fn experiments_back_off_when_they_fail() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut link = Link::new(t0);
        link.update(t0, Health::Congested, false, Layer::High);
        assert_eq!(link.cap, Layer::Low);
        // Not before the wait, and not while strained.
        link.update(t0 + s(3), Health::Good, false, Layer::High);
        assert_eq!(link.cap, Layer::Low);
        link.update(t0 + s(5), Health::Good, false, Layer::High);
        assert_eq!(link.cap, Layer::High, "first experiment after 4 s");
        // It brings congestion: back down, and the next waits 8 s.
        link.update(t0 + s(6), Health::Congested, false, Layer::High);
        assert_eq!(link.cap, Layer::Low);
        link.update(t0 + s(13), Health::Good, false, Layer::High);
        assert_eq!(link.cap, Layer::Low);
        link.update(t0 + s(15), Health::Good, false, Layer::High);
        assert_eq!(link.cap, Layer::High);
        // This one holds: past the trial it stays, and nothing climbs past
        // what was asked.
        link.update(t0 + s(22), Health::Good, false, Layer::High);
        assert_eq!(link.cap, Layer::High);
        let mut asked_low = Link::new(t0);
        asked_low.update(t0, Health::Congested, false, Layer::Low);
        asked_low.update(t0, Health::Congested, false, Layer::Low);
        assert_eq!(asked_low.cap, Layer::Off);
        asked_low.update(t0 + s(5), Health::Good, false, Layer::Low);
        assert_eq!(asked_low.cap, Layer::Low);
        asked_low.update(t0 + s(20), Health::Good, false, Layer::Low);
        assert_eq!(asked_low.cap, Layer::Low);
    }

    #[test]
    fn the_budget_grows_slowly_and_falls_fast() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut b = Budget::default();
        // Held back by the budget and nobody complains: it grows.
        b.update(t0, 3_000, 0, 3, true);
        assert_eq!(b.kbps, 3_187);
        // Not held back: it stays.
        b.update(t0 + ms(1000), 3_000, 0, 3, false);
        assert_eq!(b.kbps, 3_187);
        // Most receivers congested: down to 85 % of what went out.
        b.update(t0 + ms(2000), 3_000, 2, 3, true);
        assert_eq!(b.kbps, 2_550);
        // The next report still describes the past: no second cut yet,
        // and no growth for a while either.
        b.update(t0 + ms(3000), 2_500, 2, 3, true);
        assert_eq!(b.kbps, 2_550);
        b.update(t0 + ms(4000), 2_500, 0, 3, true);
        assert_eq!(b.kbps, 2_550);
        b.update(t0 + ms(5500), 2_500, 0, 3, true);
        assert_eq!(b.kbps, 2_709);
        // One congested receiver out of three is that receiver's problem.
        b.update(t0 + ms(9000), 2_500, 1, 3, true);
        assert_eq!(b.kbps, 2_709);
        // It never falls below one small copy.
        b.update(t0 + ms(20_000), 0, 3, 3, true);
        assert_eq!(b.kbps, MIN_BUDGET);
        assert!(shared_congestion(1, 1));
        assert!(!shared_congestion(1, 3));
        assert!(!shared_congestion(0, 0));
    }

    #[test]
    fn the_videos_shown_are_the_ones_that_matter() {
        let now = Instant::now();
        let ago = |s: u64| Some(now - Duration::from_secs(s));
        let c = |uid, zoomed, screen, spoke| Candidate {
            uid,
            zoomed,
            screen,
            spoke,
        };
        let cands = [
            c(1, false, false, ago(100)),
            c(2, false, true, None),
            c(3, false, false, ago(2)),
            c(4, true, false, None),
            c(5, false, false, None),
            c(6, false, false, None),
        ];
        let none = HashSet::new();
        let picked = pick_videos(&cands, &none, 3);
        // The enlarged one, the screen, the last to speak.
        assert_eq!(picked, HashSet::from([4, 2, 3]));
        // Of the silent ones, whoever is shown already stays.
        let picked = pick_videos(&cands[4..], &HashSet::from([6]), 1);
        assert_eq!(picked, HashSet::from([6]));
        // Someone who just spoke takes the place of someone who has not.
        let mut cands2 = cands;
        cands2[4].spoke = ago(0);
        let picked = pick_videos(&cands2, &HashSet::from([1, 3, 6]), 4);
        assert!(picked.contains(&5) && !picked.contains(&1), "{picked:?}");
        assert_eq!(pick_videos(&cands, &none, 99).len(), cands.len());
    }

    #[test]
    fn copies_follow_the_quality() {
        let q = Quality::by_key("1080p60");
        let screen = copies(Video::Screen, q);
        assert_eq!((screen.high.width, screen.high.fps), (1920, 60));
        assert_eq!(screen.high.max_kbps, q.screen);
        assert!(screen.low.width < screen.high.width);
        let camera = copies(Video::Camera, q);
        assert_eq!(camera.high.max_kbps, q.camera);
        assert!(camera.low.max_kbps < camera.high.min_kbps);
    }
}
