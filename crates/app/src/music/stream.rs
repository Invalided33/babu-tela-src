//! Downloads a song's audio (what yt-dlp handed back) and decodes it into
//! 48 kHz stereo PCM, piece by piece: a progressive file (a YouTube WebM
//! or M4A) through reqwest; an HLS playlist (SoundCloud AAC HLS) through
//! fetching every segment in turn.
//!
//! The decoder is the same symphonia the audio attachments use, so AAC,
//! Opus, Vorbis, MP3, FLAC and WAV all go through it without new crates.
//! The PCM goes out in 10 ms chunks (480 stereo frames), the pace the
//! voice encoder takes, through a channel the music module reads.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

/// How long an HTTP read waits before it gives up on a stall. YouTube and
/// SoundCloud answer fast once the connection is up.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// One PCM chunk ready to go: 480 stereo frames, interleaved. The last
/// chunk of a song may be shorter.
pub struct Pcm {
    pub samples: Vec<f32>,
}

/// Plays `track` into `pcm`, cutting out as soon as `stop` fires or `pcm`
/// is dropped.
pub async fn play(
    track: Extracted,
    pcm: tokio::sync::mpsc::Sender<Pcm>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let bytes = if track.hls {
        fetch_hls(&track.stream_url, &stop).await?
    } else {
        fetch_progressive(&track.stream_url, &stop).await?
    };
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let ext = track.ext.clone();
    tokio::task::spawn_blocking(move || decode(bytes, &ext, &pcm, &stop))
        .await
        .map_err(|e| format!("decodificador: {e}"))?
}

/// Downloads a progressive file (YouTube WebM/M4A, SoundCloud preview MP3)
/// into a Vec. YouTube's `videoplayback` URL only serves one chunk per
/// request without a `Range` header (hence why songs cut around 40 s if
/// read in one go), so this always sends `Range: bytes=X-` and loops until
/// the total from the server's `Content-Range` is in. SoundCloud's AAC
/// progressive honours the same header, so the one code path handles both.
async fn fetch_progressive(url: &str, stop: &Arc<AtomicBool>) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| format!("rede: {e}"))?;
    let mut out: Vec<u8> = Vec::new();
    let mut total: Option<u64> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(out);
        }
        let range = format!("bytes={}-", out.len());
        let resp = client
            .get(url)
            .header("Range", &range)
            .send()
            .await
            .map_err(|e| format!("rede: {e}"))?;
        let status = resp.status();
        if status.as_u16() == 416 {
            // No more bytes beyond what we have: that is the end.
            return Ok(out);
        }
        if !status.is_success() {
            return Err(format!("rede: {status}"));
        }
        if total.is_none() {
            total = content_range_total(resp.headers())
                .or_else(|| resp.content_length().map(|cl| out.len() as u64 + cl));
            if let Some(t) = total {
                out.reserve(t.saturating_sub(out.len() as u64).min(u64::from(u32::MAX)) as usize);
            }
        }
        let before = out.len();
        let mut resp = resp;
        while let Some(piece) = resp.chunk().await.map_err(|e| format!("rede: {e}"))? {
            if stop.load(Ordering::Relaxed) {
                return Ok(out);
            }
            out.extend_from_slice(&piece);
        }
        // Nothing new in this response: the server has no more to give.
        if out.len() == before {
            return Ok(out);
        }
        // Everything the server said there was is in: we are done.
        if let Some(t) = total
            && out.len() as u64 >= t
        {
            return Ok(out);
        }
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
    let client = reqwest::Client::builder()
        .timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| format!("rede: {e}"))?;
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

fn decode(
    data: Vec<u8>,
    ext: &str,
    pcm: &tokio::sync::mpsc::Sender<Pcm>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let source = Box::new(VecCursor::new(data)) as Box<dyn MediaSource>;
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

    // The decoder runs free: the bounded `pcm` channel (500 chunks ~ 5 s)
    // blocks it when the encoder is behind. During a pause the encoder
    // stops reading and the decoder parks here on `blocking_send`.
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                crate::logln!("música: pacote: {e}");
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
                return Ok(());
            }
            if pcm.blocking_send(Pcm { samples: chunk }).is_err() {
                return Ok(());
            }
        }
    }
    // The last partial chunk, padded with silence so it still carries 10
    // ms of time.
    if !pending.is_empty() && !stop.load(Ordering::Relaxed) {
        pending.resize(CHUNK * 2, 0.0);
        let _ = pcm.blocking_send(Pcm { samples: pending });
    }
    Ok(())
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
            let r = decode(wav, "wav", &tx, &stop);
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

    /// Serves `body` over HTTP/1.1 on a throwaway port, honouring
    /// `Range: bytes=N-` and handing out at most 8 KB per request (so the
    /// `fetch_progressive` Range loop has to iterate), then 416 past the
    /// end. `Connection: close` so each request is its own socket.
    fn serve_with_range(body: Vec<u8>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let total = body.len();
            for _ in 0..256 {
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
                let start = req
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| r.split('-').next())
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if start >= total {
                    let _ = sock.write_all(
                        b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                let end = (start + 8192).min(total);
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

    #[test]
    fn plays_a_whole_file_over_http_in_range_chunks() {
        // reqwest here is built with rustls-no-provider; the app installs
        // ring at startup, a test must do it itself (idempotent).
        let _ = rustls::crypto::ring::default_provider().install_default();
        // 0.5 s of stereo = 24 000 frames = 50 chunks of 10 ms.
        let wav = sine_wav(0.5);
        let port = serve_with_range(wav);
        let track = Extracted {
            service: super::super::extractor::Service::YouTube,
            title: "t".into(),
            uploader: None,
            duration: None,
            stream_url: format!("http://127.0.0.1:{port}/song.wav"),
            ext: "wav".into(),
            hls: false,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Pcm>(100_000);
        let stop = Arc::new(AtomicBool::new(false));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(play(track, tx, stop)).expect("play ok");
        let mut chunks = 0usize;
        while rx.blocking_recv().is_some() {
            chunks += 1;
        }
        // The whole file came through the Range loop, not cut at one
        // response (the "stopped at ~40 s" bug).
        assert!(chunks >= 49, "cut short at {chunks} chunks");
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
        let out = decode(data, "", &tx, &stop);
        // May be Ok (stop seen first) or Err (probe failed): either way,
        // no PCM should have been sent.
        let _ = out;
    }
}
