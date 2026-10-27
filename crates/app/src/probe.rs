//! Measurements for `cargo xtask medir`. With BABUTELA_MEDIR=1 the app
//! times what it does (how long a message takes to arrive, a voice to be
//! heard, a picture to be seen) and the test report sums it up. Otherwise
//! every call here is one load of a flag and nothing else.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex, OnceLock, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// The switch.
const ENV: &str = "BABUTELA_MEDIR";

/// Samples kept per series: plenty for a run of a few minutes.
const MAX_SAMPLES: usize = 100_000;

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(ENV).is_some_and(|v| v == "1"))
}

/// Milliseconds since 1970, to the microsecond. Every process of this PC
/// reads the same clock, so one copy of the app can stamp something and
/// another read the stamp.
pub fn wall_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

static SERIES: LazyLock<Mutex<BTreeMap<String, Vec<f64>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static COUNTS: LazyLock<Mutex<BTreeMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
/// The phase of the run (the last `mark` step): what is measured from
/// then on is kept apart, as "phase:series".
static PHASE: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

/// When `main` began (set by its first line), for the time to the first
/// frame.
pub static STARTED: OnceLock<Instant> = OnceLock::new();

/// The stamp is drawn while a whole screen is shared too (the smoke test's
/// `carimbo:on`).
pub static STAMP_SCREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// From now on, measurements belong to `label`.
pub fn phase(label: &str) {
    if on() {
        *PHASE.lock().unwrap_or_else(PoisonError::into_inner) = label.to_owned();
    }
}

fn key(series: &str) -> String {
    let phase = PHASE.lock().unwrap_or_else(PoisonError::into_inner);
    if phase.is_empty() {
        series.to_owned()
    } else {
        format!("{phase}:{series}")
    }
}

/// One measurement (milliseconds, unless the series says otherwise).
pub fn record(series: &'static str, value: f64) {
    if !on() || !value.is_finite() {
        return;
    }
    let key = key(series);
    let mut all = SERIES.lock().unwrap_or_else(PoisonError::into_inner);
    let v = all.entry(key).or_default();
    if v.len() < MAX_SAMPLES {
        v.push(value);
    }
}

/// The time since `start`, in milliseconds.
pub fn since(series: &'static str, start: Instant) {
    if on() {
        record(series, start.elapsed().as_secs_f64() * 1000.0);
    }
}

/// Something happened `n` more times.
pub fn count(name: &'static str, n: u64) {
    if !on() {
        return;
    }
    let key = key(name);
    *COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_default() += n;
}

/// `BABUTELA_TESTE_PERDA=envio:N` loses the first N messages sent through
/// the gateway on their way out; `eco:N` loses the server's copy of them
/// instead. Either way HTTP has to take over, once and only once.
pub fn lose(what: &str) -> bool {
    static LEFT: LazyLock<Mutex<Option<(String, u32)>>> = LazyLock::new(|| {
        let asked = std::env::var("BABUTELA_TESTE_PERDA").ok().and_then(|v| {
            let (kind, n) = v.split_once(':')?;
            Some((kind.to_owned(), n.parse().ok()?))
        });
        Mutex::new(asked)
    });
    if !on() {
        return false;
    }
    let mut left = LEFT.lock().unwrap_or_else(PoisonError::into_inner);
    match left.as_mut() {
        Some((kind, n)) if kind == what && *n > 0 => {
            *n -= 1;
            count("teste.perdidas", 1);
            true
        }
        _ => false,
    }
}

/// The messages the tests time say when they were sent: `medir:<ms>:<n>`.
pub fn message_stamp(text: &str) -> Option<f64> {
    if !on() {
        return None;
    }
    text.strip_prefix("medir:")?.split(':').next()?.parse().ok()
}

/// Every series in percentiles, and the counters, for the test report.
pub fn summary() -> Value {
    let series: serde_json::Map<String, Value> = SERIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(name, v)| (name.clone(), stats(v)))
        .collect();
    let counts: serde_json::Map<String, Value> = COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(name, n)| (name.clone(), json!(n)))
        .collect();
    json!({ "series": series, "counts": counts })
}

fn stats(v: &[f64]) -> Value {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let pick = |q: f64| s[((s.len() - 1) as f64 * q).round() as usize];
    let round = |x: f64| (x * 100.0).round() / 100.0;
    let mean = s.iter().sum::<f64>() / s.len() as f64;
    json!({
        "n": s.len(),
        "mean": round(mean),
        "min": round(s[0]),
        "p50": round(pick(0.5)),
        "p90": round(pick(0.9)),
        "p95": round(pick(0.95)),
        "p99": round(pick(0.99)),
        "max": round(s[s.len() - 1]),
    })
}

/// The stamp a sharing app draws across the top of its window: the time,
/// as a row of black and white cells, so whoever receives the picture can
/// tell how old it is. Two guard cells on each side, 32 bits of the time
/// in milliseconds, and 8 of check.
pub mod stamp {
    pub const CELLS: usize = 44;

    /// Of the window's height, how much the row takes (and at least 20
    /// points).
    pub const BAND: f32 = 0.03;

    fn check(low: u32) -> u8 {
        low.to_be_bytes().iter().fold(0xa5, |acc, b| acc ^ b)
    }

    /// The cells for `ms` (true is white).
    pub fn cells(ms: f64) -> [bool; CELLS] {
        let low = (ms as u64 & 0xffff_ffff) as u32;
        let mut out = [false; CELLS];
        out[0] = true;
        for i in 0..32 {
            out[2 + i] = (low >> (31 - i)) & 1 == 1;
        }
        let c = check(low);
        for i in 0..8 {
            out[34 + i] = (c >> (7 - i)) & 1 == 1;
        }
        out[43] = true;
        out
    }

    /// Reads a stamp back (`cell(i)` says whether cell i is white), as the
    /// full time near `now_ms`. None when the row is not a stamp.
    pub fn read(cell: impl Fn(usize) -> bool, now_ms: f64) -> Option<f64> {
        if !cell(0) || cell(1) || cell(42) || !cell(43) {
            return None;
        }
        let low = (0..32).fold(0u32, |acc, i| (acc << 1) | u32::from(cell(2 + i)));
        let c = (0..8).fold(0u8, |acc, i| (acc << 1) | u8::from(cell(34 + i)));
        if c != check(low) {
            return None;
        }
        let now = now_ms as u64;
        let behind = (now as u32).wrapping_sub(low);
        Some(now.wrapping_sub(u64::from(behind)) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_reads_back_as_the_time_it_was_drawn() {
        let drawn = 1_793_000_123_456.0;
        let cells = stamp::cells(drawn);
        let read = stamp::read(|i| cells[i], drawn + 80.0);
        assert_eq!(read, Some(drawn));
        // A flipped bit is caught by the check.
        let mut bad = cells;
        bad[10] = !bad[10];
        assert_eq!(stamp::read(|i| bad[i], drawn + 80.0), None);
        // Anything that is not a stamp (all black, all white) is no time.
        assert_eq!(stamp::read(|_| false, drawn), None);
        assert_eq!(stamp::read(|_| true, drawn), None);
    }

    #[test]
    fn percentiles_of_a_series() {
        let s = stats(&(1..=100).map(f64::from).collect::<Vec<_>>());
        assert_eq!(s["n"], 100);
        assert_eq!(s["p50"], 51.0);
        assert_eq!(s["p95"], 95.0);
        assert_eq!(s["max"], 100.0);
    }
}
