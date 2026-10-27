//! Audio files, decoded in Rust: symphonia reads MP3, M4A (AAC), OGG
//! (Vorbis), WAV and FLAC, and the Opus library the calls already use
//! decodes Opus (OGG/WebM voice notes, what WhatsApp sends).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::packet::Packet;
use symphonia::core::units::{Time, TimeBase};

use super::output::Feed;
use super::{Cmd, Shared};
use crate::sounds::Sounds;

enum Decoder {
    Symphonia(Box<dyn AudioDecoder>),
    Opus { dec: opus::Decoder, channels: usize },
}

impl Decoder {
    fn new(params: &AudioCodecParameters) -> Result<Decoder, String> {
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
            let dec = opus::Decoder::new(48_000, layout).map_err(|e| e.to_string())?;
            return Ok(Decoder::Opus { dec, channels });
        }
        symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map(Decoder::Symphonia)
            .map_err(|e| format!("codec: {e}"))
    }

    /// Decodes one packet onto `out` (interleaved), returning its rate and
    /// channel count. A damaged packet is skipped, as players do.
    fn decode(&mut self, packet: &Packet, out: &mut Vec<f32>) -> Option<(u32, u16)> {
        match self {
            Decoder::Symphonia(d) => {
                let buf = d.decode(packet).ok()?;
                let spec = (buf.spec().rate(), buf.spec().channels().count() as u16);
                let start = out.len();
                out.resize(start + buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut out[start..]);
                Some(spec)
            }
            Decoder::Opus { dec, channels } => {
                // 120 ms is the longest an Opus packet gets.
                let mut pcm = vec![0f32; 5760 * *channels];
                let n = dec.decode_float(&packet.data, &mut pcm, false).ok()?;
                out.extend_from_slice(&pcm[..n * *channels]);
                Some((48_000, *channels as u16))
            }
        }
    }

    fn reset(&mut self) {
        match self {
            Decoder::Symphonia(d) => d.reset(),
            Decoder::Opus { dec, .. } => {
                let _ = dec.reset_state();
            }
        }
    }
}

/// Plays from `source`: a file of this PC, or the server's copy read in
/// pieces as it goes (super::remote).
pub fn run(
    source: Box<dyn MediaSource>,
    ext: &str,
    shared: &Arc<Shared>,
    cmds: &Receiver<Cmd>,
    sounds: &Arc<Sounds>,
) {
    if let Err(e) = play(source, ext, shared, cmds, sounds) {
        shared.fail(e);
    }
}

fn play(
    source: Box<dyn MediaSource>,
    ext: &str,
    shared: &Arc<Shared>,
    cmds: &Receiver<Cmd>,
    sounds: &Arc<Sounds>,
) -> Result<(), String> {
    let source = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if !ext.is_empty() {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("formato: {e}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("o arquivo não tem som")?;
    let id = track.id;
    let time_base = track.time_base;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("o arquivo não tem som")?
        .clone();
    let duration = track
        .duration
        .zip(time_base)
        .and_then(|(d, tb)| tb.calc_duration(d))
        .map(|t| t.as_secs_f64())
        .or_else(|| {
            track
                .num_frames
                .zip(params.sample_rate)
                .map(|(n, r)| n as f64 / f64::from(r.max(1)))
        });
    let mut decoder = Decoder::new(&params)?;

    // The rate and channels come from the first sound decoded: some
    // containers do not say, or say wrong.
    let mut samples = Vec::new();
    let mut spec = None;
    while spec.is_none() {
        match format.next_packet() {
            Ok(Some(p)) if p.track_id == id => spec = decoder.decode(&p, &mut samples),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Err("nada para tocar".into()),
        }
    }
    let (rate, channels) = spec.unwrap_or((48_000, 2));
    let feed = Feed::new(rate, channels, shared.wake.clone());
    if !sounds.add_source(feed.source()) {
        return Err("sem saída de som".into());
    }
    feed.push(samples.drain(..));
    shared.ready(duration, None, Some(feed.clone()));

    let mut at_end = false;
    loop {
        let cmd = if at_end {
            shared
                .decoded_all
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // Nothing to do until a seek (playing again) or the end.
            Some(cmds.recv().unwrap_or(Cmd::Quit))
        } else {
            match cmds.try_recv() {
                Ok(cmd) => Some(cmd),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Cmd::Quit),
            }
        };
        match cmd {
            Some(Cmd::Seek(t)) => {
                let landed = seek(format.as_mut(), id, time_base, t);
                decoder.reset();
                shared.sought(landed);
                at_end = false;
                continue;
            }
            Some(Cmd::Quit) => {
                feed.finish();
                return Ok(());
            }
            None => {}
        }
        if !feed.wants_more() {
            // A second ahead: until the sound runs low or a command comes.
            shared.wake.wait();
            continue;
        }
        match format.next_packet() {
            Ok(Some(p)) if p.track_id == id => {
                decoder.decode(&p, &mut samples);
                feed.push(samples.drain(..));
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => at_end = true,
        }
    }
}

/// Moves to `t` seconds and returns where it landed (the start of a packet,
/// a little before `t`), or `t` if the file cannot say.
fn seek(format: &mut dyn FormatReader, id: u32, time_base: Option<TimeBase>, t: f64) -> f64 {
    let time = Time::try_from_secs_f64(t.max(0.0)).unwrap_or_default();
    format
        .seek(
            SeekMode::Coarse,
            SeekTo::Time {
                time,
                track_id: Some(id),
            },
        )
        .ok()
        .and_then(|s| time_base.and_then(|tb| tb.calc_time(s.actual_ts)))
        .map_or(t, |t| t.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// A WAV made here: a second of a 440 Hz tone, 8 kHz mono.
    fn wav() -> Vec<u8> {
        let rate = 8000u32;
        let samples: Vec<i16> = (0..rate)
            .map(|i| {
                ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 12000.0) as i16
            })
            .collect();
        let data_len = samples.len() as u32 * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn decodes_a_wav_with_its_length() {
        let source = MediaSourceStream::new(Box::new(Cursor::new(wav())), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("wav");
        let mut format = symphonia::default::get_probe()
            .probe(
                &hint,
                source,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .unwrap();
        let track = format.default_track(TrackType::Audio).unwrap();
        let id = track.id;
        let params = track
            .codec_params
            .as_ref()
            .unwrap()
            .audio()
            .unwrap()
            .clone();
        let mut dec = Decoder::new(&params).unwrap();
        let mut out = Vec::new();
        let mut spec = None;
        while let Ok(Some(p)) = format.next_packet() {
            if p.track_id == id {
                spec = dec.decode(&p, &mut out).or(spec);
            }
        }
        assert_eq!(spec, Some((8000, 1)));
        assert_eq!(out.len(), 8000);
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.3 && peak < 0.4, "{peak}");

        // Back to the middle: it lands at most a packet before, and what
        // follows is the rest of the file.
        let tb = format.default_track(TrackType::Audio).unwrap().time_base;
        let landed = seek(format.as_mut(), id, tb, 0.5);
        assert!((0.4..=0.5).contains(&landed), "{landed}");
        dec.reset();
        let mut rest = Vec::new();
        while let Ok(Some(p)) = format.next_packet() {
            dec.decode(&p, &mut rest);
        }
        let expected = ((1.0 - landed) * 8000.0).round() as usize;
        assert!(
            rest.len().abs_diff(expected) <= 1,
            "{} {expected}",
            rest.len()
        );
    }

    /// What the calls send is what a voice note holds: Opus, decoded here.
    #[test]
    fn decodes_opus_packets() {
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let tone: Vec<f32> = (0..960).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let packet = enc.encode_vec_float(&tone, 4000).unwrap();
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_OPUS)
            .with_channels(symphonia::core::audio::Channels::Discrete(1));
        let mut dec = Decoder::new(&params).unwrap();
        let p = Packet::new(0, Default::default(), Default::default(), packet);
        let mut out = Vec::new();
        assert_eq!(dec.decode(&p, &mut out), Some((48_000, 1)));
        assert_eq!(out.len(), 960);
    }
}
