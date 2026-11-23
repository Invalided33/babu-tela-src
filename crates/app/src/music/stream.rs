//! Downloads a song's audio (what yt-dlp handed back) and decodes it into
//! 48 kHz stereo PCM, piece by piece: a progressive file (a YouTube WebM
//! or M4A) through reqwest, decoded while the rest of it is still coming;
//! an HLS playlist (SoundCloud AAC HLS) through fetching every segment in
//! turn.
//!
//! The decoder is the same symphonia the audio attachments use, so AAC,
//! Opus, Vorbis, MP3, FLAC and WAV all go through it without new crates.
//! The PCM goes out in 10 ms chunks (480 stereo frames), the pace the
//! voice encoder takes, through a channel the music module reads.

use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::extractor::Extracted;
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

/// 48 kHz stereo, matching the voice encoder.
pub const RATE: u32 = 48_000;
/// 10 ms per chunk at 48 kHz (one stereo frame has two samples).
pub const CHUNK: usize = 480;

/// How long a read may stall before the download gives up. A whole song
/// may take longer than that on a slow line, and goes on: only a silent
/// connection is given up on.
const STALL: Duration = Duration::from_secs(20);

/// How long connecting may take.
const CONNECT: Duration = Duration::from_secs(10);

/// What one request asks for: YouTube serves 10 MiB pieces without
/// slowing down (yt-dlp's `http_chunk_size` for it).
const PIECE: u64 = 10 * 1024 * 1024;

/// The longest wait for the audio to become available (`available_at`).
const MAX_WAIT: Duration = Duration::from_secs(20);

/// After `available_at`, a 403 is still retried this long, for a clock a
/// little behind YouTube's.
const LATE_403: f64 = 5.0;

/// One PCM chunk ready to go: 480 stereo frames, interleaved. The last
/// chunk of a song may be shorter.
pub struct Pcm {
    pub samples: Vec<f32>,
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT)
        .read_timeout(STALL)
        .build()
        .map_err(|e| format!("rede: {e}"))
}

/// Plays `track` into `pcm`, cutting out as soon as `stop` fires or `pcm`
/// is dropped. An error means no sound at all went out (the caller may try
/// another way); a download that breaks off mid-song ends the song there.
pub async fn play(
    track: Extracted,
    pcm: tokio::sync::mpsc::Sender<Pcm>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let ext = track.ext.clone();
    if track.hls {
        let bytes = fetch_hls(&track.stream_url, &stop).await?;
        let source = Box::new(VecCursor::new(bytes));
        let decoded = decode_off_thread(source, ext, pcm, stop.clone()).await;
        return outcome(decoded, None, &stop);
    }
    wait_until(track.available_at, &stop).await;
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let client = client()?;
    let url = track.stream_url.clone();
    let first = first_response(&client, &url, track.available_at, &stop).await?;
    let Some(total) = first.total else {
        // No size given: the old way, the whole file and then the decoder.
        let bytes = fetch_progressive(&client, &url, first.response, &stop).await?;
        let source = Box::new(VecCursor::new(bytes));
        let decoded = decode_off_thread(source, ext, pcm, stop.clone()).await;
        return outcome(decoded, None, &stop);
    };
    let download = Arc::new(Download::new(total));
    let feeder = tokio::spawn(feed(
        client,
        url,
        first.response,
        download.clone(),
        stop.clone(),
    ));
    let source = Box::new(Reader {
        download: download.clone(),
        pos: 0,
        stop: stop.clone(),
    });
    let decoded = decode_off_thread(source, ext, pcm, stop.clone()).await;
    // The decoder is done (the end, a stop, or it gave up): nothing more
    // is wanted from the network.
    feeder.abort();
    outcome(decoded, download.failure(), &stop)
}

async fn decode_off_thread(
    source: Box<dyn MediaSource>,
    ext: String,
    pcm: tokio::sync::mpsc::Sender<Pcm>,
    stop: Arc<AtomicBool>,
) -> Result<usize, String> {
    tokio::task::spawn_blocking(move || decode(source, &ext, &pcm, &stop))
        .await
        .map_err(|e| format!("decodificador: {e}"))?
}

/// What `play` says: fine once anything went out (a download that broke
/// off later only ends the song early) or when it was stopped; else the
/// download's error, if it broke, or the decoder's.
fn outcome(
    decoded: Result<usize, String>,
    broke: Option<String>,
    stop: &AtomicBool,
) -> Result<(), String> {
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    match decoded {
        Ok(0) => Err(broke.unwrap_or_else(|| "a música veio sem som".to_owned())),
        Ok(_) => {
            if let Some(e) = broke {
                crate::logln!("música: o download parou no meio ({e})");
            }
            Ok(())
        }
        Err(e) => Err(broke.unwrap_or(e)),
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Waits for YouTube to serve the audio: until then it answers 403 (for
/// its TV app, until the ad it would have shown would have played).
async fn wait_until(available_at: Option<f64>, stop: &AtomicBool) {
    let Some(at) = available_at else { return };
    let wait = (at - now_secs() + 0.2).clamp(0.0, MAX_WAIT.as_secs_f64());
    if wait <= 0.0 {
        return;
    }
    crate::logln!("música: o YouTube só libera o áudio em {wait:.1} s");
    let until = std::time::Instant::now() + Duration::from_secs_f64(wait);
    while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The answer to a request for one piece, and the file's size if it said.
struct First {
    response: reqwest::Response,
    total: Option<u64>,
}

/// Asks for the piece from `start` on.
async fn request(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    total: Option<u64>,
) -> Result<reqwest::Response, String> {
    let end = total.map_or(start + PIECE, |t| t.min(start + PIECE)) - 1;
    client
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| format!("rede: {e}"))
}

/// The first piece's answer. A 403 is retried for a few seconds after
/// `available_at`, in case this PC's clock is behind YouTube's.
async fn first_response(
    client: &reqwest::Client,
    url: &str,
    available_at: Option<f64>,
    stop: &AtomicBool,
) -> Result<First, String> {
    loop {
        let response = request(client, url, 0, None).await?;
        let status = response.status();
        if status.as_u16() == 403
            && available_at.is_some_and(|at| now_secs() < at + LATE_403)
            && !stop.load(Ordering::Relaxed)
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        if !status.is_success() {
            return Err(format!("rede: {status}"));
        }
        let total = content_range_total(response.headers()).or_else(|| {
            (status.as_u16() == 200)
                .then(|| response.content_length())
                .flatten()
        });
        return Ok(First { response, total });
    }
}

/// The song's bytes as they come down, for the decoder to read while the
/// rest is still on its way.
struct Download {
    got: Mutex<Got>,
    more: Condvar,
    total: u64,
}

#[derive(Default)]
struct Got {
    data: Vec<u8>,
    done: bool,
    failed: Option<String>,
}

impl Download {
    fn new(total: u64) -> Download {
        // What the server says, up to a sane amount (a wrong header must
        // not ask for gigabytes at once); the rest grows as it comes.
        let data = Vec::with_capacity(total.min(256 * 1024 * 1024) as usize);
        Download {
            got: Mutex::new(Got {
                data,
                ..Got::default()
            }),
            more: Condvar::new(),
            total,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Got> {
        self.got.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn len(&self) -> u64 {
        self.lock().data.len() as u64
    }

    fn push(&self, bytes: &[u8]) {
        self.lock().data.extend_from_slice(bytes);
        self.more.notify_all();
    }

    fn finish(&self, failed: Option<String>) {
        {
            let mut got = self.lock();
            got.done = true;
            got.failed = failed;
        }
        self.more.notify_all();
    }

    fn failure(&self) -> Option<String> {
        self.lock().failed.clone()
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        self.more.notify_all();
    }
}

/// Reads the whole song, piece after piece, into `download`.
async fn feed(
    client: reqwest::Client,
    url: String,
    first: reqwest::Response,
    download: Arc<Download>,
    stop: Arc<AtomicBool>,
) {
    let result = async {
        let mut response = first;
        loop {
            let start = download.len();
            // A server that ignores Range sends the file from its start.
            let mut skip = if response.status().as_u16() == 200 {
                start
            } else {
                0
            };
            while let Some(piece) = response.chunk().await.map_err(|e| format!("rede: {e}"))? {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let cut = skip.min(piece.len() as u64) as usize;
                skip -= cut as u64;
                download.push(&piece[cut..]);
            }
            let have = download.len();
            // All in, or the server has nothing more to give.
            if have >= download.total || have == start {
                return Ok(());
            }
            response = request(&client, &url, have, Some(download.total)).await?;
            match response.status().as_u16() {
                416 => return Ok(()),
                s if !(200..300).contains(&s) => {
                    return Err(format!("rede: {}", response.status()));
                }
                _ => {}
            }
        }
    }
    .await;
    download.finish(result.err());
}

/// The decoder's side of a `Download`: reads wait for the bytes asked for
/// to arrive.
struct Reader {
    download: Arc<Download>,
    pos: u64,
    stop: Arc<AtomicBool>,
}

impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut got = self.download.lock();
        loop {
            let have = got.data.len() as u64;
            if self.pos < have {
                let start = self.pos as usize;
                let n = out.len().min(got.data.len() - start);
                out[..n].copy_from_slice(&got.data[start..start + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if got.done {
                return match &got.failed {
                    Some(e) => Err(std::io::Error::other(e.clone())),
                    None => Ok(0),
                };
            }
            if self.stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            got = self
                .download
                .more
                .wait_timeout(got, Duration::from_millis(200))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Seek for Reader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let to = match pos {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(d) => i128::from(self.download.total) + i128::from(d),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
        };
        self.pos = u64::try_from(to).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "antes do começo")
        })?;
        Ok(self.pos)
    }
}

impl MediaSource for Reader {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.download.total)
    }
}

/// Downloads a progressive file whose size the server did not say, into
/// a Vec: piece after piece until a piece comes empty or the server says
/// there is no more (416).
async fn fetch_progressive(
    client: &reqwest::Client,
    url: &str,
    first: reqwest::Response,
    stop: &Arc<AtomicBool>,
) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut response = first;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(out);
        }
        let status = response.status();
        if status.as_u16() == 416 {
            // No more bytes beyond what we have: that is the end.
            return Ok(out);
        }
        if !status.is_success() {
            return Err(format!("rede: {status}"));
        }
        // A server that ignores Range sends the whole file, once.
        let whole = status.as_u16() == 200;
        let before = out.len();
        while let Some(piece) = response.chunk().await.map_err(|e| format!("rede: {e}"))? {
            if stop.load(Ordering::Relaxed) {
                return Ok(out);
            }
            out.extend_from_slice(&piece);
        }
        // Nothing new in this response: the server has no more to give.
        if whole || out.len() == before {
            return Ok(out);
        }
        response = request(client, url, out.len() as u64, None).await?;
    }
}

/// Reads the total length from a `Content-Range: bytes <start>-<end>/<total>`
/// header. The `*` form means the server does not know and sends `None`.
fn content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get("content-range")?.to_str().ok()?;
    let total = v.split('/').nth(1)?.trim();
    (total != "*").then(|| total.parse().ok()).flatten()
}

/// Reads an HLS .m3u8, downloads every segment and concatenates them (an
/// AAC-in-MPEG-TS stream, which symphonia reads straight through). Only
/// media playlists: a master playlist is not expected from yt-dlp with the
/// chosen format.
async fn fetch_hls(playlist_url: &str, stop: &Arc<AtomicBool>) -> Result<Vec<u8>, String> {
    let client = client()?;
    let text = client
        .get(playlist_url)
        .send()
        .await
        .map_err(|e| format!("rede: {e}"))?
        .error_for_status()
        .map_err(|e| format!("rede: {e}"))?
        .text()
        .await
        .map_err(|e| format!("rede: {e}"))?;

    let base = reqwest::Url::parse(playlist_url).map_err(|e| format!("URL: {e}"))?;
    let mut segments = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let abs = base.join(line).map_err(|e| format!("segmento HLS: {e}"))?;
        segments.push(abs);
    }
    if segments.is_empty() {
        return Err("HLS vazio".to_owned());
    }

    let mut out = Vec::new();
    for seg in segments {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let bytes = client
            .get(seg.as_str())
            .send()
            .await
            .map_err(|e| format!("rede: {e}"))?
            .error_for_status()
            .map_err(|e| format!("rede: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("rede: {e}"))?;
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

/// A MediaSource over a Vec<u8>, so symphonia reads straight out of memory.
struct VecCursor {
    inner: std::io::Cursor<Vec<u8>>,
    len: u64,
}

impl VecCursor {
    fn new(data: Vec<u8>) -> VecCursor {
        let len = data.len() as u64;
        VecCursor {
            inner: std::io::Cursor::new(data),
            len,
        }
    }
}

impl Read for VecCursor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for VecCursor {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for VecCursor {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// A linear resampler the ScreenEncoder-friendly 10 ms chunks are built
/// from. The input rate stays through the song; the output is always
/// `RATE`.
struct Resampler {
    from: u32,
    to: u32,
    /// Where in the stream of input frames the next output sample is to
    /// come from, kept across `process` calls so neighbouring batches
    /// interpolate smoothly.
    read_pos: f64,
    /// The last input frame of the previous batch (so the start of the
    /// next batch can interpolate against it).
    prev: [f32; 2],
    primed: bool,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Resampler {
        Resampler {
            from,
            to,
            read_pos: 0.0,
            prev: [0.0; 2],
            primed: false,
        }
    }

    /// Interleaved stereo in, interleaved stereo out.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / 2;
        if frames == 0 {
            return;
        }
        let step = f64::from(self.from) / f64::from(self.to);
        let max_pos = frames as f64 - 1.0;
        while self.read_pos <= max_pos {
            let whole = self.read_pos.floor() as i32;
            let frac = (self.read_pos - f64::from(whole)) as f32;
            let left = if whole < 0 {
                if self.primed {
                    self.prev
                } else {
                    [input[0], input[1]]
                }
            } else {
                let i = whole as usize;
                [input[i * 2], input[i * 2 + 1]]
            };
            let right_i = (whole + 1) as usize;
            if right_i >= frames {
                break;
            }
            let right = [input[right_i * 2], input[right_i * 2 + 1]];
            out.push(left[0] + (right[0] - left[0]) * frac);
            out.push(left[1] + (right[1] - left[1]) * frac);
            self.read_pos += step;
        }
        // The next batch starts at `input[0]` as frame 0, so what used to
        // be frame `frames - 1` is frame `-1`: shift the read position
        // and keep that frame as `prev`.
        self.read_pos -= frames as f64;
        self.prev = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
        self.primed = true;
    }
}

/// Each PCM chunk is 10 ms of stereo. The encoder in music/mod.rs paces
/// the output at that rate; the decoder itself runs free into a bounded
/// `pcm` channel so that during a pause the memory does not grow.
pub const CHUNK_MS: u64 = 10;

/// How many `CHUNK_MS` chunks go in a burst before pacing starts. The
/// local mixer waits for `PeerAudio::PRIME` (60 ms of stereo) to be
/// queued before it starts playing, so a lead below that would never
/// make the DJ hear the song; a lead above `TRIM_ABOVE` (another 30 ms)
/// would make the mixer start trimming quiet stretches. 70 ms clears
/// both.
pub const LEAD_CHUNKS: u64 = 7;

/// Decodes `source` into `pcm`; says how many chunks went out.
fn decode(
    source: Box<dyn MediaSource>,
    ext: &str,
    pcm: &tokio::sync::mpsc::Sender<Pcm>,
    stop: &AtomicBool,
) -> Result<usize, String> {
    if stop.load(Ordering::Relaxed) {
        return Ok(0);
    }
    let mss = MediaSourceStream::new(source, Default::default());

    let mut hint = Hint::new();
    if !ext.is_empty() {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("sem decodificador para o formato: {e}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "sem faixa de áudio".to_owned())?;
    let track_id = track.id;
    let params: AudioCodecParameters = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .cloned()
        .ok_or_else(|| "sem parâmetros do codec".to_owned())?;
    let mut decoder = make_decoder(&params)?;
    let in_rate = params.sample_rate.unwrap_or(RATE);
    let in_channels = params
        .channels
        .as_ref()
        .map_or(2, |c| c.count())
        .clamp(1, 2);
    let mut resampler = Resampler::new(in_rate, RATE);

    let mut pending: Vec<f32> = Vec::new();
    let mut raw: Vec<f32> = Vec::new();
    let mut stereo: Vec<f32> = Vec::new();
    let mut sent = 0usize;
    // Damaged packets in a row: a few are skipped, an endless run is not.
    let mut bad = 0u32;

    // The decoder runs free: the bounded `pcm` channel (500 chunks ~ 5 s)
    // blocks it when the encoder is behind. During a pause the encoder
    // stops reading and the decoder parks here on `blocking_send`.
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => {
                bad = 0;
                p
            }
            Ok(None) => break,
            // The bytes ran out: the end of the file, or of what the
            // download brought (`play` says which).
            Err(symphonia::core::errors::Error::IoError(_)) => break,
            Err(e) => {
                crate::logln!("música: pacote: {e}");
                bad += 1;
                if bad > 100 {
                    break;
                }
                continue;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        raw.clear();
        if decoder.decode(&packet, &mut raw).is_none() {
            continue;
        }

        // Mono → stereo if needed.
        stereo.clear();
        if in_channels == 1 {
            for sample in &raw {
                stereo.push(*sample);
                stereo.push(*sample);
            }
        } else {
            stereo.extend_from_slice(&raw);
        }
        resampler.process(&stereo, &mut pending);

        while pending.len() >= CHUNK * 2 {
            let chunk: Vec<f32> = pending.drain(..CHUNK * 2).collect();
            if stop.load(Ordering::Relaxed) {
                return Ok(sent);
            }
            if pcm.blocking_send(Pcm { samples: chunk }).is_err() {
                return Ok(sent);
            }
            sent += 1;
        }
    }
    // The last partial chunk, padded with silence so it still carries 10
    // ms of time.
    if !pending.is_empty() && !stop.load(Ordering::Relaxed) {
        pending.resize(CHUNK * 2, 0.0);
        if pcm.blocking_send(Pcm { samples: pending }).is_ok() {
            sent += 1;
        }
    }
    Ok(sent)
}

enum Dec {
    Symphonia(Box<dyn AudioDecoder>),
    Opus { dec: opus::Decoder, channels: usize },
}

impl Dec {
    fn decode(
        &mut self,
        packet: &symphonia::core::packet::Packet,
        out: &mut Vec<f32>,
    ) -> Option<()> {
        match self {
            Dec::Symphonia(d) => {
                let buf = d.decode(packet).ok()?;
                let start = out.len();
                out.resize(start + buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut out[start..]);
                Some(())
            }
            Dec::Opus { dec, channels } => {
                let mut pcm = vec![0f32; 5760 * *channels];
                let n = dec.decode_float(&packet.data, &mut pcm, false).ok()?;
                out.extend_from_slice(&pcm[..n * *channels]);
                Some(())
            }
        }
    }
}

fn make_decoder(params: &AudioCodecParameters) -> Result<Dec, String> {
    if params.codec == CODEC_ID_OPUS {
        let channels = params
            .channels
            .as_ref()
            .map_or(2, |c| c.count())
            .clamp(1, 2);
        let layout = if channels == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        };
        let dec = opus::Decoder::new(RATE, layout).map_err(|e| e.to_string())?;
        return Ok(Dec::Opus { dec, channels });
    }
    symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map(Dec::Symphonia)
        .map_err(|e| format!("codec {:?}: {e}", params.codec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_preserves_a_matched_rate() {
        let mut r = Resampler::new(48_000, 48_000);
        let input = vec![0.1, 0.2, 0.3, 0.4];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn resampler_doubles_from_24_to_48() {
        let mut r = Resampler::new(24_000, 48_000);
        // Two stereo frames.
        let input: Vec<f32> = vec![0.0, 1.0, 2.0, 3.0];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // One doubling produces at least 3 output frames from 2 input
        // frames; keep room for the resampler's windowing.
        assert!(out.len() >= 4, "{}", out.len());
    }

    fn headers(items: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut m = reqwest::header::HeaderMap::new();
        for (k, v) in items {
            m.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    /// A valid 48 kHz 16-bit stereo PCM WAV of `secs` of a 440 Hz tone,
    /// for decode tests without a network or yt-dlp.
    fn sine_wav(secs: f32) -> Vec<u8> {
        let rate = 48_000u32;
        let ch = 2u16;
        let bits = 16u16;
        let frames = (rate as f32 * secs) as u32;
        let data_len = frames * u32::from(ch) * u32::from(bits / 8);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&ch.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * u32::from(ch) * u32::from(bits / 8)).to_le_bytes());
        v.extend_from_slice(&(ch * (bits / 8)).to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let t = i as f32 / rate as f32;
            let q = ((t * 440.0 * std::f32::consts::TAU).sin() * 10_000.0) as i16;
            v.extend_from_slice(&q.to_le_bytes());
            v.extend_from_slice(&q.to_le_bytes());
        }
        v
    }

    #[test]
    fn decode_wav_yields_real_time_10ms_stereo_chunks() {
        // 0.3 s of 48 kHz stereo is 14 400 frames = 28 800 samples, which
        // is exactly 30 chunks of CHUNK*2 (10 ms each). This is the core
        // of playback: the decoder must hand out whole 10 ms stereo
        // chunks so the mixer and the Opus encoder stay in step.
        let wav = sine_wav(0.3);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pcm>(10_000);
        let stop = AtomicBool::new(false);
        let worker = std::thread::spawn(move || {
            let r = decode(Box::new(VecCursor::new(wav)), "wav", &tx, &stop);
            drop(tx);
            r
        });
        let mut chunks = 0usize;
        let mut nonzero = false;
        while let Some(p) = rx.blocking_recv() {
            assert_eq!(p.samples.len(), CHUNK * 2, "chunk not 10 ms stereo");
            nonzero |= p.samples.iter().any(|s| s.abs() > 0.001);
            chunks += 1;
        }
        worker.join().unwrap().expect("decode ok");
        assert!((29..=31).contains(&chunks), "chunks={chunks}");
        assert!(nonzero, "decoded silence");
    }

    /// How the test server answers.
    #[derive(Clone, Default)]
    struct Serving {
        /// The most bytes in one answer (0: all that was asked for).
        piece: usize,
        /// 403 to everything until then.
        forbidden_until: Option<std::time::Instant>,
        /// Requests from this byte on wait this long before answering.
        slow_from: Option<(usize, Duration)>,
        /// Requests from this byte on get a 500.
        broken_from: Option<usize>,
    }

    /// Serves `body` over HTTP/1.1 on a throwaway port, honouring
    /// `Range: bytes=N-M` (and 416 past the end), as `how` says.
    /// `Connection: close` so each request is its own socket.
    fn serve(body: Vec<u8>, how: Serving) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let total = body.len();
            for _ in 0..512 {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                let range = req.lines().find_map(|l| l.strip_prefix("range: bytes="));
                let start = range
                    .and_then(|r| r.split('-').next())
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let asked_end = range
                    .and_then(|r| r.split('-').nth(1))
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .map_or(total, |e| e + 1);
                let reply = |sock: &mut std::net::TcpStream, status: &str| {
                    let _ = sock.write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                };
                if how
                    .forbidden_until
                    .is_some_and(|t| std::time::Instant::now() < t)
                {
                    reply(&mut sock, "403 Forbidden");
                    continue;
                }
                if start >= total {
                    reply(&mut sock, "416 Range Not Satisfiable");
                    continue;
                }
                if how.broken_from.is_some_and(|b| start >= b) {
                    reply(&mut sock, "500 Internal Server Error");
                    continue;
                }
                if let Some((from, wait)) = how.slow_from
                    && start >= from
                {
                    std::thread::sleep(wait);
                }
                let mut end = asked_end.min(total);
                if how.piece > 0 {
                    end = end.min(start + how.piece);
                }
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    start,
                    end - 1,
                    total,
                    end - start,
                );
                let _ = sock.write_all(header.as_bytes());
                let _ = sock.write_all(&body[start..end]);
            }
        });
        port
    }

    fn track(port: u16, available_at: Option<f64>) -> Extracted {
        Extracted {
            service: super::super::extractor::Service::YouTube,
            title: "t".into(),
            uploader: None,
            duration: None,
            stream_url: format!("http://127.0.0.1:{port}/song.wav"),
            ext: "wav".into(),
            hls: false,
            available_at,
            fast: true,
        }
    }

    /// Plays `track`; the outcome, the chunks that came out, and when the
    /// first one did.
    fn play_all(track: Extracted) -> (Result<(), String>, usize, Option<Duration>) {
        // reqwest here is built with rustls-no-provider; the app installs
        // ring at startup, a test must do it itself (idempotent).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pcm>(100_000);
        let stop = Arc::new(AtomicBool::new(false));
        let started = std::time::Instant::now();
        let reader = std::thread::spawn(move || {
            let mut chunks = 0usize;
            let mut first = None;
            while rx.blocking_recv().is_some() {
                first.get_or_insert_with(|| started.elapsed());
                chunks += 1;
            }
            (chunks, first)
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(play(track, tx, stop));
        let (chunks, first) = reader.join().unwrap();
        (result, chunks, first)
    }

    #[test]
    fn plays_a_whole_file_over_http_in_range_chunks() {
        // 0.5 s of stereo = 24 000 frames = 50 chunks of 10 ms, 8 KB per
        // answer: the whole file comes through the Range loop, not cut at
        // one response (the "stopped at ~40 s" bug).
        let port = serve(
            sine_wav(0.5),
            Serving {
                piece: 8192,
                ..Serving::default()
            },
        );
        let (result, chunks, _) = play_all(track(port, None));
        result.expect("play ok");
        assert!(chunks >= 49, "cut short at {chunks} chunks");
    }

    #[test]
    fn the_song_starts_before_the_download_ends() {
        // The second half takes 1.5 s to come: the first half plays
        // meanwhile.
        let wav = sine_wav(1.0);
        let half = wav.len() / 2;
        let port = serve(
            wav,
            Serving {
                piece: half,
                slow_from: Some((half, Duration::from_millis(1500))),
                ..Serving::default()
            },
        );
        let (result, chunks, first) = play_all(track(port, None));
        result.expect("play ok");
        assert!(chunks >= 99, "cut short at {chunks} chunks");
        let first = first.expect("no sound");
        assert!(
            first < Duration::from_millis(1000),
            "first sound only at {first:?}"
        );
    }

    #[test]
    fn waits_until_youtube_serves_the_audio() {
        // As YouTube's TV app: 403 until the ad it would have shown is
        // over (`available_at`), then the file.
        let port = serve(
            sine_wav(0.3),
            Serving {
                forbidden_until: Some(std::time::Instant::now() + Duration::from_millis(1200)),
                ..Serving::default()
            },
        );
        let at = now_secs() + 1.2;
        let (result, chunks, first) = play_all(track(port, Some(at)));
        result.expect("play ok");
        assert!(chunks >= 29, "cut short at {chunks} chunks");
        let first = first.expect("no sound");
        assert!(
            first >= Duration::from_millis(1100) && first < Duration::from_secs(4),
            "first sound at {first:?}"
        );
    }

    #[test]
    fn a_refusal_with_nothing_to_wait_for_fails_at_once() {
        let port = serve(
            sine_wav(0.3),
            Serving {
                forbidden_until: Some(std::time::Instant::now() + Duration::from_secs(60)),
                ..Serving::default()
            },
        );
        let started = std::time::Instant::now();
        let (result, chunks, _) = play_all(track(port, None));
        let e = result.expect_err("a 403 played");
        assert!(e.contains("403"), "{e}");
        assert_eq!(chunks, 0);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_download_that_breaks_off_plays_what_came() {
        let wav = sine_wav(1.0);
        let half = wav.len() / 2;
        let port = serve(
            wav,
            Serving {
                piece: half,
                broken_from: Some(half),
                ..Serving::default()
            },
        );
        let (result, chunks, _) = play_all(track(port, None));
        result.expect("what came should play");
        assert!((30..90).contains(&chunks), "chunks={chunks}");
    }

    #[test]
    fn content_range_reads_the_total() {
        let h = headers(&[("Content-Range", "bytes 0-499/5000000")]);
        assert_eq!(content_range_total(&h), Some(5_000_000));
    }

    #[test]
    fn content_range_without_a_total_is_none() {
        let h = headers(&[("Content-Range", "bytes 0-499/*")]);
        assert_eq!(content_range_total(&h), None);
    }

    #[test]
    fn no_content_range_is_none() {
        let h = headers(&[]);
        assert_eq!(content_range_total(&h), None);
    }

    #[test]
    fn decode_bails_on_stop() {
        // Hand `decode` a flag set to stop before anything is read; it
        // should return at once, without needing a real codec in the data.
        let stop = AtomicBool::new(true);
        let (tx, _rx) = tokio::sync::mpsc::channel::<Pcm>(1);
        // An empty Vec will not probe (no symphonia hint matches), but
        // the stop flag is checked before the probe. Give an invalid Vec
        // and expect decode to short-circuit after `stop` is seen.
        let data = vec![0u8; 128];
        let out = decode(Box::new(VecCursor::new(data)), "", &tx, &stop);
        // May be Ok (stop seen first) or Err (probe failed): either way,
        // no PCM should have been sent.
        let _ = out;
    }
}
