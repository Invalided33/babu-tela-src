//! Puts pictures back together from their RTP packets, H.264 (RFC 6184,
//! packetization mode 1) or AV1 (the AOM's RTP payload format), and hands
//! each one over the moment its last packet is in. A general-purpose
//! sample builder waits for the first packet of the next picture to know
//! the last one is whole, which costs a whole frame interval (a lot more on
//! a still screen, where the next picture may be half a second away). The
//! marker bit already says a picture ended, and the sequence numbers say
//! whether anything is missing.
//!
//! A missing packet holds the pictures after it (each depends on the one
//! before) until it is retransmitted (NACK), a full picture arrives, or
//! `patience` runs out; then the broken ones are dropped and a full picture
//! is asked for.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rtc::rtp::codec::av1::Av1Depacketizer;
use rtc::rtp::packetizer::Depacketizer;

use super::plan::Codec;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Packets kept at most while waiting (a few big full pictures' worth).
const MAX_WAITING: usize = 4096;

struct Waiting {
    marker: bool,
    payload: Bytes,
    at: Instant,
}

/// One whole picture, as the decoder takes it.
pub struct Picture {
    /// H.264: Annex B, each NAL unit after a start code. AV1: the temporal
    /// unit's OBUs, each with its size.
    pub data: Vec<u8>,
    /// A full picture (it starts with the stream's parameters, or the
    /// sequence header, and is a key frame).
    pub key: bool,
    /// When its first packet arrived.
    pub first_at: Instant,
}

/// What one packet led to.
#[derive(Default)]
pub struct Pushed {
    pub pictures: Vec<Picture>,
    /// Pictures were lost: a full one is needed to carry on.
    pub lost: bool,
}

pub struct Assembler {
    packets: BTreeMap<u64, Waiting>,
    /// The highest sequence number seen, extended past 16 bits.
    highest: Option<u64>,
    /// Where the next picture starts. None until the first full picture
    /// (and again after a loss nothing could heal).
    next: Option<u64>,
    patience: Duration,
    codec: Codec,
}

impl Assembler {
    pub fn new(patience: Duration, codec: Codec) -> Assembler {
        Assembler {
            packets: BTreeMap::new(),
            highest: None,
            next: None,
            patience,
            codec,
        }
    }

    /// The 16-bit sequence number, extended by the wraps seen so far.
    fn extend(&mut self, seq: u16) -> u64 {
        let ext = match self.highest {
            // Far from zero, so numbers just below the first stay positive.
            None => (1u64 << 32) + u64::from(seq),
            Some(h) => {
                let delta = i64::from(seq.wrapping_sub(h as u16) as i16);
                (h as i64 + delta) as u64
            }
        };
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        ext
    }

    /// One packet: its sequence number, marker bit and payload.
    pub fn push(&mut self, seq: u16, marker: bool, payload: Bytes, now: Instant) -> Pushed {
        let mut out = Pushed::default();
        let ext = self.extend(seq);
        if self.next.is_some_and(|n| ext < n) || payload.is_empty() {
            // Already handed over (or skipped): a late copy.
            return out;
        }
        self.packets.entry(ext).or_insert(Waiting {
            marker,
            payload,
            at: now,
        });
        loop {
            if self.next.is_none() {
                // Out of step: only a full picture can start things again.
                match self.key_start(u64::MIN) {
                    Some(start) => {
                        self.drop_before(start);
                        self.next = Some(start);
                    }
                    None => {
                        self.trim();
                        return out;
                    }
                }
            }
            let Some(next) = self.next else {
                return out;
            };
            if let Some(end) = self.complete_from(next) {
                out.pictures.extend(self.take(end));
                self.next = Some(end + 1);
                continue;
            }
            // Something is missing in the picture that comes next. A whole
            // full picture after it makes it unnecessary.
            if let Some(start) = self
                .key_start(next + 1)
                .filter(|s| self.complete_from(*s).is_some())
            {
                self.drop_before(start);
                self.next = Some(start);
                continue;
            }
            let oldest = self.packets.values().map(|p| p.at).min();
            let waited_too_long =
                oldest.is_some_and(|at| now.saturating_duration_since(at) > self.patience);
            if waited_too_long || self.packets.len() > MAX_WAITING {
                // Given up: what is here is useless until a full picture.
                out.lost = true;
                match self.key_start(next + 1) {
                    Some(start) => {
                        self.drop_before(start);
                        self.next = Some(start);
                        continue;
                    }
                    None => {
                        self.packets.clear();
                        self.next = None;
                    }
                }
            }
            return out;
        }
    }

    /// The end (the marker packet) of the picture that starts at `from`,
    /// when every packet of it is in.
    fn complete_from(&self, from: u64) -> Option<u64> {
        for (expected, (&seq, p)) in (from..).zip(self.packets.range(from..)) {
            if seq != expected {
                return None;
            }
            if p.marker {
                return Some(seq);
            }
        }
        None
    }

    /// The first packet at or after `from` that starts a full picture.
    fn key_start(&self, from: u64) -> Option<u64> {
        self.packets
            .range(from..)
            .find(|(_, p)| starts_key(self.codec, &p.payload))
            .map(|(&seq, _)| seq)
    }

    fn drop_before(&mut self, seq: u64) {
        self.packets = self.packets.split_off(&seq);
    }

    /// Keeps the waiting list bounded while out of step.
    fn trim(&mut self) {
        while self.packets.len() > MAX_WAITING {
            self.packets.pop_first();
        }
    }

    /// Takes the packets up to `to` out as one picture (none if they do not
    /// make sense in the codec). Nothing before the picture is left waiting.
    fn take(&mut self, to: u64) -> Option<Picture> {
        let rest = self.packets.split_off(&(to + 1));
        let mine = std::mem::replace(&mut self.packets, rest);
        let mut data = Vec::with_capacity(mine.values().map(|p| p.payload.len() + 4).sum());
        let key = match self.codec {
            Codec::H264 => {
                let mut key = false;
                for p in mine.values() {
                    key |= depacketize(&p.payload, &mut data)?;
                }
                key
            }
            Codec::Av1 => {
                let key = mine
                    .values()
                    .next()
                    .is_some_and(|p| starts_key(Codec::Av1, &p.payload));
                // A fresh one for each picture: a picture starts with a
                // whole OBU, and leftovers of a lost one must not join it.
                let mut obus = Av1Depacketizer::new();
                for p in mine.values() {
                    data.extend_from_slice(&obus.depacketize(&p.payload).ok()?);
                }
                key
            }
        };
        Some(Picture {
            data,
            key,
            first_at: mine.values().map(|p| p.at).min()?,
        })
    }
}

/// Whether a payload starts a full picture. H.264: an SPS, alone or first
/// in a STAP-A (the way the sending side packs the parameters before an
/// IDR). AV1: an aggregation header that starts a coded video sequence (N)
/// with a whole OBU (no Z): the sequence header leads, and the sending side
/// sends it with key frames only (hw.rs `tidy_av1`).
fn starts_key(codec: Codec, payload: &[u8]) -> bool {
    match codec {
        Codec::H264 => match payload.first().map(|b| b & 0x1f) {
            Some(7) => true,
            Some(24) => payload.get(3).is_some_and(|b| b & 0x1f == 7),
            _ => false,
        },
        Codec::Av1 => payload
            .first()
            .is_some_and(|b| b & 0x80 == 0 && b & 0x08 != 0),
    }
}

/// Appends one payload's NAL units to `out` in Annex B. Returns whether it
/// carried an IDR slice, or None for what packetization mode 1 does not
/// send.
fn depacketize(payload: &[u8], out: &mut Vec<u8>) -> Option<bool> {
    let header = *payload.first()?;
    match header & 0x1f {
        kind @ 1..=23 => {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(payload);
            Some(kind == 5)
        }
        // STAP-A: two-byte sizes, each followed by a NAL unit.
        24 => {
            let mut rest = &payload[1..];
            let mut key = false;
            while rest.len() >= 2 {
                let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                let nal = rest.get(2..2 + len)?;
                if let Some(b) = nal.first() {
                    key |= b & 0x1f == 5;
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(nal);
                }
                rest = &rest[2 + len..];
            }
            Some(key)
        }
        // FU-A: one NAL unit in pieces; the first rebuilds its header.
        28 => {
            let fu = *payload.get(1)?;
            if fu & 0x80 != 0 {
                out.extend_from_slice(&START_CODE);
                out.push((header & 0xe0) | (fu & 0x1f));
            }
            out.extend_from_slice(&payload[2..]);
            Some(fu & 0x80 != 0 && fu & 0x1f == 5)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 4] = [0x67, 0x42, 0xc0, 0x1f];
    const PPS: [u8; 3] = [0x68, 0xce, 0x3c];

    /// The packets of one picture, as the sending side's packetizer makes
    /// them: the parameters in a STAP-A, then the slice in FU-A pieces of
    /// `piece` bytes.
    fn packets(key: bool, slice_len: usize, piece: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if key {
            let mut stap = vec![0x78];
            for nal in [&SPS[..], &PPS[..]] {
                stap.extend((nal.len() as u16).to_be_bytes());
                stap.extend_from_slice(nal);
            }
            out.push(stap);
        }
        let header = if key { 0x65 } else { 0x41 };
        let body: Vec<u8> = (0..slice_len).map(|i| (i % 251) as u8).collect();
        let pieces: Vec<&[u8]> = body.chunks(piece).collect();
        for (i, chunk) in pieces.iter().enumerate() {
            let mut fu = vec![(header & 0xe0) | 28, header & 0x1f];
            if i == 0 {
                fu[1] |= 0x80;
            }
            if i == pieces.len() - 1 {
                fu[1] |= 0x40;
            }
            fu.extend_from_slice(chunk);
            out.push(fu);
        }
        out
    }

    struct Feed {
        a: Assembler,
        seq: u16,
        now: Instant,
    }

    impl Feed {
        fn new(first_seq: u16) -> Feed {
            Feed {
                a: Assembler::new(Duration::from_millis(200), Codec::H264),
                seq: first_seq,
                now: Instant::now(),
            }
        }

        /// Numbers the packets of a picture (stamped `ts`), without sending.
        fn number(&mut self, pkts: Vec<Vec<u8>>, ts: u32) -> Vec<(u16, u32, bool, Bytes)> {
            let n = pkts.len();
            pkts.into_iter()
                .enumerate()
                .map(|(i, p)| {
                    let seq = self.seq;
                    self.seq = self.seq.wrapping_add(1);
                    (seq, ts, i == n - 1, Bytes::from(p))
                })
                .collect()
        }

        fn send(&mut self, p: &(u16, u32, bool, Bytes)) -> Pushed {
            self.a.push(p.0, p.2, p.3.clone(), self.now)
        }
    }

    #[test]
    fn a_picture_comes_out_with_its_last_packet() {
        let mut f = Feed::new(100);
        let key = f.number(packets(true, 3000, 1000), 9000);
        for p in &key[..key.len() - 1] {
            assert!(f.send(p).pictures.is_empty());
        }
        let out = f.send(&key[key.len() - 1]);
        assert_eq!(out.pictures.len(), 1);
        let pic = &out.pictures[0];
        assert!(pic.key);
        // SPS, PPS and the IDR rebuilt with its header, each after a start code.
        assert_eq!(&pic.data[..8], &[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f]);
        let idr = pic.data.windows(5).position(|w| w == [0, 0, 0, 1, 0x65]);
        assert!(idr.is_some());
        assert_eq!(pic.data.len(), 4 + 4 + 4 + 3 + 4 + 1 + 3000);
        // The next one, a small single-packet picture, at once too.
        let p = f.number(vec![vec![0x41, 1, 2, 3]], 12000);
        let out = f.send(&p[0]);
        assert_eq!(out.pictures.len(), 1);
        assert!(!out.pictures[0].key);
        assert_eq!(out.pictures[0].data, [0, 0, 0, 1, 0x41, 1, 2, 3]);
    }

    #[test]
    fn nothing_comes_out_before_a_full_picture() {
        let mut f = Feed::new(65530);
        let p = f.number(packets(false, 2500, 1000), 1);
        for pkt in &p {
            let out = f.send(pkt);
            assert!(out.pictures.is_empty() && !out.lost);
        }
        // Across the wrap of the sequence numbers, too.
        let k = f.number(packets(true, 2500, 1000), 2);
        let mut got = 0;
        for pkt in &k {
            got += f.send(pkt).pictures.len();
        }
        assert_eq!(got, 1);
    }

    #[test]
    fn a_reordered_packet_completes_the_picture_when_it_lands() {
        let mut f = Feed::new(7);
        let k = f.number(packets(true, 4000, 1000), 1);
        let (first, rest) = k.split_at(2);
        for p in first {
            f.send(p);
        }
        // The last two cross on the way.
        assert!(f.send(&rest[rest.len() - 1]).pictures.is_empty());
        for p in &rest[..rest.len() - 1] {
            let out = f.send(p);
            if p.0 == rest[rest.len() - 2].0 {
                assert_eq!(out.pictures.len(), 1);
            }
        }
    }

    #[test]
    fn a_lost_packet_holds_what_follows_until_given_up() {
        let mut f = Feed::new(1);
        for p in f.number(packets(true, 2000, 1000), 1) {
            f.send(&p);
        }
        let broken = f.number(packets(false, 3000, 1000), 2);
        let after = f.number(vec![vec![0x41, 9]], 3);
        // The second piece of the next picture never arrives.
        for (i, p) in broken.iter().enumerate() {
            if i != 1 {
                assert!(f.send(p).pictures.is_empty());
            }
        }
        let out = f.send(&after[0]);
        assert!(out.pictures.is_empty() && !out.lost, "esperou pelo pedaço");
        // Too long: given up, and a full picture is needed.
        f.now += Duration::from_millis(300);
        let late = f.number(vec![vec![0x41, 10]], 4);
        let out = f.send(&late[0]);
        assert!(out.pictures.is_empty());
        assert!(out.lost);
        // The full picture that comes then starts everything again.
        let k = f.number(packets(true, 1500, 1000), 5);
        let mut got = Vec::new();
        for p in &k {
            got.extend(f.send(p).pictures);
        }
        assert_eq!(got.len(), 1);
        assert!(got[0].key);
    }

    #[test]
    fn a_full_picture_after_a_loss_skips_the_wait() {
        let mut f = Feed::new(500);
        for p in f.number(packets(true, 2000, 1000), 1) {
            f.send(&p);
        }
        let broken = f.number(packets(false, 3000, 1000), 2);
        for (i, p) in broken.iter().enumerate() {
            if i != 0 {
                f.send(p);
            }
        }
        let k = f.number(packets(true, 2000, 1000), 3);
        let mut got = Vec::new();
        for p in &k {
            got.extend(f.send(p).pictures);
        }
        assert_eq!(got.len(), 1, "a imagem completa saiu sem esperar");
        assert!(got[0].key);
        // The missing piece arriving late changes nothing.
        assert!(f.send(&broken[0]).pictures.is_empty());
    }

    #[test]
    fn late_copies_are_ignored() {
        let mut f = Feed::new(40);
        let k = f.number(packets(true, 1000, 1000), 1);
        for p in &k {
            f.send(p);
        }
        for p in &k {
            let out = f.send(p);
            assert!(out.pictures.is_empty() && !out.lost);
        }
    }
}
