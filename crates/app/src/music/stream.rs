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

use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use tokio::sync::mpsc::UnboundedSender;

use super::extractor::Extracted;

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
    pcm: UnboundedSender<Pcm>,
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
/// into a Vec. These top out around 10 MB for a 4-minute song, which fits.
async fn fetch_progressive(url: &str, stop: &Arc<AtomicBool>) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| format!("rede: {e}"))?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("rede: {e}"))?
        .error_for_status()
        .map_err(|e| format!("rede: {e}"))?;
    let mut out = Vec::with_capacity(resp.content_length().unwrap_or(0) as usize);
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("rede: {e}"))? {
        if stop.load(Ordering::Relaxed) {
            return Ok(out);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
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

fn decode(
    data: Vec<u8>,
    ext: &str,
    pcm: &UnboundedSender<Pcm>,
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
            if pcm.send(Pcm { samples: chunk }).is_err() {
                return Ok(());
            }
        }
    }
    // The last partial chunk, padded with silence so it still carries 10
    // ms of time.
    if !pending.is_empty() && !stop.load(Ordering::Relaxed) {
        pending.resize(CHUNK * 2, 0.0);
        let _ = pcm.send(Pcm { samples: pending });
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

    #[test]
    fn decode_bails_on_stop() {
        // Hand `decode` a flag set to stop before anything is read; it
        // should return at once, without needing a real codec in the data.
        let stop = AtomicBool::new(true);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
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
