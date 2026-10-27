//! Where a player's sound goes: a queue the decoder fills and the sound
//! card drains (through the same mixer as the app's cues), and the clock
//! that follows it, so pictures show when their sound is heard.

use std::collections::VecDeque;
use std::num::NonZero;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::Wake;

/// How far ahead of the sound card the decoder keeps the sound, in
/// seconds: it reads more while less than this is queued (`wants_more`),
/// and once this much is, waits for the queue to drop below it again.
const AHEAD: f64 = 1.0;

pub struct Feed {
    rate: u32,
    channels: u16,
    queue: Mutex<VecDeque<f32>>,
    /// The decoder's: rung when the queue runs low, is emptied or ends.
    wake: Arc<Wake>,
    /// Samples (all channels counted) that went to the sound card since the
    /// last seek.
    played: AtomicU64,
    paused: AtomicBool,
    /// The player is gone: the sound ends and leaves the mixer.
    done: AtomicBool,
    volume: AtomicU32,
}

impl Feed {
    pub fn new(rate: u32, channels: u16, wake: Arc<Wake>) -> Arc<Feed> {
        Arc::new(Feed {
            rate: rate.max(1),
            channels: channels.max(1),
            queue: Mutex::new(VecDeque::new()),
            wake,
            played: AtomicU64::new(0),
            paused: AtomicBool::new(true),
            done: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
        })
    }

    /// Samples (every channel counted) the decoder keeps queued.
    fn ahead(&self) -> usize {
        (f64::from(self.rate) * f64::from(self.channels) * AHEAD) as usize
    }

    /// Adds decoded samples (interleaved), all of them, at once. The
    /// decoder stops reading once a second is queued, so the queue holds
    /// that, plus whatever sound a video has between the pictures read
    /// meanwhile, which never wait for room for it.
    pub fn push(&self, samples: impl IntoIterator<Item = f32>) {
        if !self.done.load(Ordering::Relaxed) {
            self.queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(samples);
        }
    }

    /// Throws away what is queued and starts counting again (a seek).
    pub fn clear(&self) {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.played.store(0, Ordering::SeqCst);
        self.wake.ring();
    }

    pub fn queued(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the decoder should read more sound: less than `AHEAD` is
    /// queued. When it is not, it waits for its `Wake`.
    pub fn wants_more(&self) -> bool {
        self.queued() < self.ahead()
    }

    /// Seconds heard since the last seek.
    pub fn heard(&self) -> f64 {
        self.played.load(Ordering::SeqCst) as f64
            / (f64::from(self.rate) * f64::from(self.channels))
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn finish(&self) {
        self.done.store(true, Ordering::SeqCst);
        self.wake.ring();
    }

    /// What the mixer plays.
    pub fn source(self: &Arc<Self>) -> FeedSource {
        FeedSource {
            feed: self.clone(),
            local: VecDeque::new(),
        }
    }
}

/// Plays a feed: silence while paused or while the decoder catches up.
pub struct FeedSource {
    feed: Arc<Feed>,
    /// Taken from the queue a batch at a time, so the lock is not taken per
    /// sample.
    local: VecDeque<f32>,
}

impl Iterator for FeedSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.feed.done.load(Ordering::Relaxed) {
            return None;
        }
        if self.feed.paused.load(Ordering::Relaxed) {
            return Some(0.0);
        }
        if self.local.is_empty() {
            let mut queue = self
                .feed
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // Whole frames only, so the channels never swap.
            let frame = usize::from(self.feed.channels);
            let before = queue.len();
            let n = (before.min(1024) / frame) * frame;
            self.local.extend(queue.drain(..n));
            drop(queue);
            // Just dropped below what the decoder keeps: it reads more. (It
            // only waits for the sound when there was at least that much.)
            let ahead = self.feed.ahead();
            if before >= ahead && before - n < ahead {
                self.feed.wake.ring();
            }
        }
        match self.local.pop_front() {
            Some(s) => {
                self.feed.played.fetch_add(1, Ordering::Relaxed);
                Some(s * f32::from_bits(self.feed.volume.load(Ordering::Relaxed)))
            }
            None => Some(0.0),
        }
    }
}

impl rodio::Source for FeedSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        NonZero::new(self.feed.channels).unwrap_or(NonZero::<u16>::MIN)
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        NonZero::new(self.feed.rate).unwrap_or(NonZero::<u32>::MIN)
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Where in the file the player is: the sound's own position when there is
/// sound, the wall clock otherwise (a silent video, or after the sound ran
/// out).
pub struct Clock {
    /// The position the last seek or pause left it at.
    base: Mutex<f64>,
    /// When the wall clock started running (None while paused).
    since: Mutex<Option<Instant>>,
}

impl Default for Clock {
    fn default() -> Self {
        Clock {
            base: Mutex::new(0.0),
            since: Mutex::new(None),
        }
    }
}

impl Clock {
    pub fn now(&self, feed: Option<&Feed>) -> f64 {
        let base = *self.base.lock().unwrap_or_else(PoisonError::into_inner);
        match feed {
            Some(f) => base + f.heard(),
            None => {
                let since = *self.since.lock().unwrap_or_else(PoisonError::into_inner);
                base + since.map_or(0.0, |s| s.elapsed().as_secs_f64())
            }
        }
    }

    /// Jumps to `t` (a seek).
    pub fn set(&self, t: f64, running: bool) {
        *self.base.lock().unwrap_or_else(PoisonError::into_inner) = t;
        *self.since.lock().unwrap_or_else(PoisonError::into_inner) = running.then(Instant::now);
    }

    /// Stops or starts the wall clock where it is. (With sound, pausing the
    /// feed is what stops time.)
    pub fn run(&self, running: bool, feed: Option<&Feed>) {
        if feed.is_some() {
            return;
        }
        let now = self.now(None);
        self.set(now, running);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_what_was_queued_and_counts_it() {
        let feed = Feed::new(4, 2, Arc::default());
        let mut src = feed.source();
        // Paused: silence, and nothing heard.
        assert_eq!(src.next(), Some(0.0));
        feed.set_paused(false);
        feed.push([0.5, -0.5, 0.25, -0.25]);
        let got: Vec<f32> = (0..4).filter_map(|_| src.next()).collect();
        assert_eq!(got, [0.5, -0.5, 0.25, -0.25]);
        // Two frames of a 4 Hz stereo stream: half a second.
        assert!((feed.heard() - 0.5).abs() < 1e-9);
        // Running dry plays silence.
        assert_eq!(src.next(), Some(0.0));
        feed.set_volume(0.5);
        feed.push([1.0, 1.0]);
        assert_eq!(src.next(), Some(0.5));
        feed.finish();
        assert_eq!(src.next(), None);
    }

    #[test]
    fn the_decoder_reads_until_a_second_is_queued() {
        let feed = Feed::new(2, 1, Arc::default());
        assert!(feed.wants_more());
        // A second ahead: two samples. What goes past it is kept.
        feed.push([1.0, 2.0, 3.0]);
        assert_eq!(feed.queued(), 3);
        assert!(!feed.wants_more());
        feed.clear();
        assert_eq!(feed.queued(), 0);
        assert!(feed.wants_more());
    }

    #[test]
    fn the_clock_follows_the_sound_or_the_wall() {
        let clock = Clock::default();
        clock.set(10.0, false);
        assert_eq!(clock.now(None), 10.0);
        clock.run(true, None);
        std::thread::sleep(Duration::from_millis(30));
        let t = clock.now(None);
        assert!(t > 10.02 && t < 11.0, "{t}");
        clock.run(false, None);
        let paused = clock.now(None);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(clock.now(None), paused);
        let feed = Feed::new(1, 1, Arc::default());
        clock.set(3.0, false);
        assert_eq!(clock.now(Some(&feed)), 3.0);
    }
}
