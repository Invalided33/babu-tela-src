//! Noise suppression by a neural network, for the microphone in a call:
//! DPDFNet (Ceva, December 2025), the causal model its paper measured above
//! the other open ones (DeepFilterNet2 and 3, GTCRN, DPCRN, DEMUCS and
//! CleanUNet among them), in its 48 kHz version with two dual-path blocks.
//! WebRTC's suppressor follows a steady noise floor and takes it off; the
//! network was trained to keep only speech, so typing, a fan, traffic or a
//! room full of people go too, and the voice loses less of itself.
//!
//! The network works on the spectrum: every 10 ms, the last 20 ms of sound
//! under a Vorbis window, 481 bins. It hands back the cleaned spectrum of a
//! frame from before, which goes back to sound and overlaps the one before
//! it. What comes out is `DELAY` late: 10 ms of the window and 40 of the
//! model's look-ahead (its mask looks two frames ahead, and its deep filter
//! two more, over the masked frames).
//!
//! The model is in the .exe (model/, its weights stored in float16; NOTICE
//! there says where it comes from, and `cargo xtask modelo` whether Ceva
//! has published a newer one) and runs in tract, in pure Rust: about
//! 2 ms per 10 ms on a Ryzen 7 5700X3D, a fifth of one core. Reading and
//! optimizing it takes a quarter of a second, once.
//!
//! A crate of its own so that a debug build of the app optimizes it all the
//! same (the workspace's Cargo.toml asks for that): tract's generic code is
//! compiled where it is used, here.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use tract_onnx::prelude::{IntoTValue, TValue, Tensor, TypedRunnableModel, TypedSimpleState};

/// dpdfnet2_48khz_hr, as model/NOTICE says.
const MODEL: &[u8] = include_bytes!("../model/dpdfnet2_48khz_hr_f16.onnx");

/// The sample rate it works at.
pub const RATE: u32 = 48_000;

/// The window: 20 ms.
const WIN: usize = 960;

/// How far it moves each time: 10 ms.
pub const HOP: usize = 480;

const BINS: usize = WIN / 2 + 1;

/// How late the cleaned sound comes out, in samples (50 ms): one hop for
/// the window, four for the model's look-ahead.
pub const DELAY: usize = 5 * HOP;

/// The network, read and optimized: one for every call, each with its own
/// memory (`Denoiser`).
pub struct Model {
    plan: Arc<TypedRunnableModel>,
    /// The network's memory before it heard anything: where its two
    /// normalizations start (from the model's metadata), and zeros.
    start: TValue,
}

impl Model {
    /// Reads and optimizes it (a quarter of a second).
    pub fn load() -> Result<Model, String> {
        Model::from_onnx(MODEL)
    }

    /// Reads and optimizes a DPDFNet model: the app's, or a new one that
    /// `cargo xtask modelo --atualizar` checks before it takes its place.
    fn from_onnx(bytes: &[u8]) -> Result<Model, String> {
        use tract_onnx::prelude::*;
        let onnx = tract_onnx::onnx();
        let proto = onnx
            .proto_model_for_read(&mut std::io::Cursor::new(bytes))
            .map_err(|e| format!("{e:#}"))?;
        let meta = |key: &str| {
            proto
                .metadata_props
                .iter()
                .find(|p| p.key == key)
                .map(|p| p.value.as_str())
                .ok_or_else(|| format!("o modelo não diz {key}"))
        };
        let size = |key: &str| -> std::result::Result<usize, String> {
            meta(key)?.parse().map_err(|e| format!("{key}: {e}"))
        };
        let values = |key: &str| -> std::result::Result<Vec<f32>, String> {
            meta(key)?
                .split(',')
                .map(|v| v.trim().parse().map_err(|e| format!("{key}: {e}")))
                .collect()
        };
        let total = size("state_size")?;
        let erb = values("erb_norm_init")?;
        let spec = values("spec_norm_init")?;
        if erb.len() != size("erb_norm_state_size")?
            || spec.len() != size("spec_norm_state_size")?
            || erb.len() + spec.len() > total
        {
            return Err("o começo da memória do modelo não confere".into());
        }
        let mut start = vec![0f32; total];
        start[..erb.len()].copy_from_slice(&erb);
        start[erb.len()..erb.len() + spec.len()].copy_from_slice(&spec);
        let plan = onnx
            .model_for_proto_model(&proto)
            .and_then(|m| m.with_input_fact(0, f32::fact([1, 1, BINS, 2]).into()))
            .and_then(|m| m.with_input_fact(1, f32::fact([total]).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(|e| format!("{e:#}"))?;
        let start = Tensor::from_shape(&[total], &start)
            .map_err(|e| format!("{e:#}"))?
            .into_tvalue();
        Ok(Model { plan, start })
    }
}

/// The Vorbis window: its square and the square of its other half add up
/// to one, so the frames overlap back into the same sound.
fn vorbis(n: usize) -> Vec<f32> {
    let half = n as f64 / 2.0;
    (0..n)
        .map(|i| {
            let s = (std::f64::consts::FRAC_PI_2 * (i as f64 + 0.5) / half).sin();
            (std::f64::consts::FRAC_PI_2 * s * s).sin() as f32
        })
        .collect()
}

/// The network at work on one microphone: its memory, and the sound
/// around each frame.
pub struct Denoiser {
    model: Arc<Model>,
    /// The plan's state as it was spawned. Each frame runs on a copy: a
    /// state that ran keeps what its recurrent layers saw, while this
    /// model takes all of its memory as an input (`memory`).
    blank: TypedSimpleState,
    memory: TValue,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    /// The last 20 ms that came in.
    input: Vec<f32>,
    frame: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// The spectrum as the model takes it: real and imaginary in pairs.
    pairs: Vec<f32>,
    time: Vec<f32>,
    /// The output being overlapped; its first 10 ms are done after each
    /// frame.
    overlap: Vec<f32>,
}

impl Denoiser {
    pub fn new(model: Arc<Model>) -> Result<Denoiser, String> {
        let blank = model.plan.spawn().map_err(|e| format!("{e:#}"))?;
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(WIN);
        let inverse = planner.plan_fft_inverse(WIN);
        let scratch = forward.get_scratch_len().max(inverse.get_scratch_len());
        Ok(Denoiser {
            memory: model.start.clone(),
            model,
            blank,
            spectrum: forward.make_output_vec(),
            scratch: vec![Complex::default(); scratch],
            forward,
            inverse,
            window: vorbis(WIN),
            input: vec![0.0; WIN],
            frame: vec![0.0; WIN],
            pairs: vec![0.0; 2 * BINS],
            time: vec![0.0; WIN],
            overlap: vec![0.0; WIN],
        })
    }

    /// As new: nothing heard, nothing remembered.
    pub fn reset(&mut self) {
        self.memory = self.model.start.clone();
        self.input.fill(0.0);
        self.overlap.fill(0.0);
    }

    /// 10 ms in (`HOP` samples at `RATE`) and 10 ms out, `DELAY` behind.
    pub fn process(&mut self, chunk: &[f32], out: &mut [f32]) -> Result<(), String> {
        if chunk.len() != HOP || out.len() != HOP {
            return Err(format!("a rede trabalha de {HOP} em {HOP} amostras"));
        }
        self.input.copy_within(HOP.., 0);
        self.input[WIN - HOP..].copy_from_slice(chunk);
        for ((f, x), w) in self.frame.iter_mut().zip(&self.input).zip(&self.window) {
            *f = x * w;
        }
        self.forward
            .process_with_scratch(&mut self.frame, &mut self.spectrum, &mut self.scratch)
            .map_err(|e| e.to_string())?;
        for (pair, c) in self
            .pairs
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(&self.spectrum)
        {
            *pair = [c.re, c.im];
        }
        let spec = Tensor::from_shape(&[1, 1, BINS, 2], &self.pairs)
            .map_err(|e| format!("{e:#}"))?
            .into_tvalue();
        let mut state = self.blank.clone();
        let mut outputs = state
            .run([spec, self.memory.clone()].into_iter().collect())
            .map_err(|e| format!("{e:#}"))?;
        // The cleaned spectrum, then the memory.
        let memory = outputs.pop();
        let cleaned = outputs.pop();
        let (Some(memory), Some(cleaned)) = (memory, cleaned) else {
            return Err("a rede não devolveu o que devia".into());
        };
        let cleaned = cleaned
            .try_as_plain_ram()
            .and_then(|c| c.as_slice::<f32>())
            .map_err(|e| format!("{e:#}"))?;
        if cleaned.len() != 2 * BINS {
            return Err("a rede devolveu um espectro de outro tamanho".into());
        }
        for (c, [re, im]) in self.spectrum.iter_mut().zip(cleaned.as_chunks::<2>().0) {
            *c = Complex::new(*re, *im);
        }
        self.memory = memory;
        // A real sound has no imaginary part at 0 Hz and at 24 kHz.
        self.spectrum[0].im = 0.0;
        self.spectrum[BINS - 1].im = 0.0;
        self.inverse
            .process_with_scratch(&mut self.spectrum, &mut self.time, &mut self.scratch)
            .map_err(|e| e.to_string())?;
        // The inverse FFT comes out WIN times too loud.
        let scale = 1.0 / WIN as f32;
        for ((o, t), w) in self.overlap.iter_mut().zip(&self.time).zip(&self.window) {
            *o += t * w * scale;
        }
        out.copy_from_slice(&self.overlap[..HOP]);
        self.overlap.copy_within(HOP.., 0);
        self.overlap[WIN - HOP..].fill(0.0);
        if !out.iter().all(|s| s.is_finite()) {
            self.reset();
            out.fill(0.0);
            return Err("a rede devolveu números inválidos".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use std::time::Instant;

    /// One model for all the tests.
    fn model() -> Arc<Model> {
        static MODEL: OnceLock<Arc<Model>> = OnceLock::new();
        MODEL
            .get_or_init(|| Arc::new(Model::load().unwrap()))
            .clone()
    }

    const SR: f64 = RATE as f64;

    /// A voice-like buzz (140 Hz with harmonics, in syllables) for two
    /// seconds out of every three.
    fn voice(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / SR;
                if t % 3.0 >= 2.0 {
                    return 0.0;
                }
                let syllable = (std::f64::consts::PI * 4.0 * t).sin().abs();
                let buzz: f64 = (1..=6)
                    .map(|h| {
                        (std::f64::consts::TAU * 140.0 * f64::from(h) * t).sin() / f64::from(h)
                    })
                    .sum();
                (0.12 * syllable * buzz) as f32
            })
            .collect()
    }

    /// White noise from a fixed seed, at an RMS of about `level`.
    fn noise(n: usize, level: f32) -> Vec<f32> {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let u = (x >> 11) as f32 / (1u64 << 53) as f32 - 0.5;
                // A uniform value in [-0.5, 0.5) has an RMS of 1/√12.
                u * level * 12f32.sqrt()
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn run(d: &mut Denoiser, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0f32; input.len()];
        let outs = out.as_chunks_mut::<HOP>().0;
        for (i, o) in input.as_chunks::<HOP>().0.iter().zip(outs) {
            d.process(i, o).unwrap();
        }
        out
    }

    #[test]
    fn the_window_overlaps_back_into_the_same_sound() {
        let w = vorbis(WIN);
        for i in 0..HOP {
            let sum = w[i] * w[i] + w[i + HOP] * w[i + HOP];
            assert!((sum - 1.0).abs() < 1e-5, "{i}: {sum}");
        }
    }

    #[test]
    fn noise_goes_and_the_voice_stays_fifty_ms_late() {
        behaves(model());
    }

    /// A new model, before `cargo xtask modelo --atualizar` puts it in the
    /// app: its float16 copy (`f16.onnx`, in the folder BABUTELA_MODELO_NOVO
    /// names) gives what the original (`original.onnx`) gives, more than
    /// 60 dB below the sound, and it does what the app's model does. Once
    /// it passed, `conferido.txt` there says by how much.
    #[test]
    #[ignore = "o cargo xtask modelo --atualizar roda este, com o modelo novo"]
    fn a_new_model_matches_its_original() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("BABUTELA_MODELO_NOVO").expect("BABUTELA_MODELO_NOVO=pasta"),
        );
        let load = |file: &str| {
            let bytes = std::fs::read(dir.join(file)).unwrap();
            Arc::new(Model::from_onnx(&bytes).unwrap_or_else(|e| panic!("{file}: {e}")))
        };
        let (original, f16) = (load("original.onnx"), load("f16.onnx"));
        let n = 6 * RATE as usize;
        let noisy: Vec<f32> = voice(n)
            .iter()
            .zip(noise(n, 0.02))
            .map(|(v, h)| v + h)
            .collect();
        let a = run(&mut Denoiser::new(original).unwrap(), &noisy);
        let b = run(&mut Denoiser::new(f16.clone()).unwrap(), &noisy);
        let apart: Vec<f32> = a.iter().zip(&b).map(|(a, b)| a - b).collect();
        let db = 20.0 * (rms(&apart) / rms(&a)).log10();
        println!(
            "o float16 fica {:.1} dB abaixo do som longe do original",
            -db
        );
        assert!(db < -60.0, "o float16 ficou a {db:.1} dB do original");
        behaves(f16);
        std::fs::write(dir.join("conferido.txt"), format!("{:.0}", -db)).unwrap();
    }

    /// What the app's model does, and a new one has to: the noise goes and
    /// the voice stays, `DELAY` late, and a reset starts it afresh.
    fn behaves(model: Arc<Model>) {
        let mut d = Denoiser::new(model.clone()).unwrap();
        let n = 6 * RATE as usize;
        let clean = voice(n);
        let hiss = noise(n, 0.02);
        let noisy: Vec<f32> = clean.iter().zip(&hiss).map(|(v, h)| v + h).collect();
        let started = Instant::now();
        let out = run(&mut d, &noisy);
        let per_hop = started.elapsed().as_secs_f64() * 1000.0 / (n / HOP) as f64;
        println!("{per_hop:.2} ms por 10 ms");

        // The delay: where the output lines up best with the clean voice
        // (over its first two seconds).
        let talk = &clean[..2 * RATE as usize];
        let lag = (0..4000)
            .step_by(10)
            .map(|d| {
                let dot: f32 = out[d..].iter().zip(talk).map(|(o, c)| o * c).sum();
                (d, dot)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(d, _)| d);
        assert_eq!(lag, Some(DELAY));

        // Seconds 2.2 to 2.8 and 5.2 to 5.8 have only the noise.
        let quiet = |s: &[f32], from: usize| {
            let a = from + RATE as usize * 22 / 10;
            s[a..a + RATE as usize * 6 / 10].to_vec()
        };
        for second in [0, 3 * RATE as usize] {
            let before = rms(&quiet(&noisy, second));
            let after = rms(&quiet(&out[DELAY..], second));
            let db = 20.0 * (after / before).log10();
            assert!(db < -30.0, "o ruído caiu só {db:.1} dB");
        }
        // The voice, half a second into talking, keeps most of its level.
        let talk = RATE as usize / 2..RATE as usize * 3 / 2;
        let kept = rms(&out[DELAY..][talk.clone()]) / rms(&clean[talk]);
        assert!((0.6..1.3).contains(&kept), "a voz ficou em {kept:.2}");

        // Afresh, it gives the same as a new one.
        d.reset();
        let mut other = Denoiser::new(model).unwrap();
        let a = run(&mut d, &noisy[..RATE as usize]);
        let b = run(&mut other, &noisy[..RATE as usize]);
        assert_eq!(a, b);
    }

    #[test]
    fn silence_stays_silence_and_the_wrong_size_is_refused() {
        let mut d = Denoiser::new(model()).unwrap();
        let out = run(&mut d, &vec![0.0; RATE as usize]);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
        let mut small = [0f32; 10];
        assert!(d.process(&[0.0; 10], &mut small).is_err());
    }
}
