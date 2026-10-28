//! The noise suppression's choice, and its network (crates/denoise:
//! DPDFNet, run in pure Rust), read once for every call.
//!
//! "IA", the default, is the network, between the echo canceller and the
//! gain: the voice comes out without the room's noise and `DELAY` (50 ms)
//! later. "Padrão" is WebRTC's suppressor, as the browsers have it, with
//! no delay; "Desligada", none. audio.rs builds the chain for the one
//! chosen, and changes it in the middle of a call too.
//!
//! The model is read on a thread of its own the first time a call wants it
//! (a quarter of a second); the call uses the standard suppressor until
//! then, and if this PC cannot run it in time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

pub use babutela_denoise::{DELAY, Denoiser, HOP, Model};
use serde::{Deserialize, Serialize};

/// Which noise suppression the microphone goes through: Configurações →
/// Voz e vídeo, kept in prefs.json.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Noise {
    /// The network.
    #[default]
    #[serde(rename = "ia")]
    Ia = 0,
    /// WebRTC's, as the browsers have it.
    #[serde(rename = "padrao")]
    Padrao = 1,
    /// None: the room as it sounds.
    #[serde(rename = "desligada")]
    Desligada = 2,
}

impl Noise {
    pub const ALL: [Noise; 3] = [Noise::Ia, Noise::Padrao, Noise::Desligada];

    pub fn label(self) -> &'static str {
        use crate::t;
        match self {
            Noise::Ia => t!("noise.ia"),
            Noise::Padrao => t!("noise.padrao"),
            Noise::Desligada => t!("noise.desligada"),
        }
    }

    /// As the tests' steps and report name it.
    pub fn key(self) -> &'static str {
        match self {
            Noise::Ia => "ia",
            Noise::Padrao => "padrao",
            Noise::Desligada => "desligada",
        }
    }

    pub fn by_key(key: &str) -> Option<Noise> {
        Noise::ALL.into_iter().find(|n| n.key() == key)
    }

    /// Back from `self as u8`.
    pub fn from_u8(v: u8) -> Noise {
        Noise::ALL.get(usize::from(v)).copied().unwrap_or_default()
    }
}

static LOADED: OnceLock<Result<Arc<Model>, String>> = OnceLock::new();
static LOADING: AtomicBool = AtomicBool::new(false);

/// Starts reading the model on a thread of its own, unless that started
/// already.
pub fn preload() {
    if LOADED.get().is_some() || LOADING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("ruido-modelo".into())
        .spawn(|| {
            let _ = load();
        });
    if spawned.is_err() {
        LOADING.store(false, Ordering::Release);
    }
}

/// The model, read now if it was not (waiting for it).
pub fn load() -> Result<Arc<Model>, String> {
    LOADED
        .get_or_init(|| {
            let started = Instant::now();
            let model = Model::load().map(Arc::new);
            match &model {
                Ok(_) => crate::logln!(
                    "voz: rede de supressão de ruído pronta em {} ms",
                    started.elapsed().as_millis()
                ),
                Err(e) => crate::logln!("voz: rede de supressão de ruído: {e}"),
            }
            model
        })
        .clone()
}

/// The model if it is ready; never waits.
pub fn ready() -> Option<Arc<Model>> {
    LOADED.get()?.as_ref().ok().cloned()
}

/// Reading it failed: the standard suppressor stays.
pub fn broken() -> bool {
    matches!(LOADED.get(), Some(Err(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_has_names_and_numbers() {
        for n in Noise::ALL {
            assert_eq!(Noise::by_key(n.key()), Some(n));
            assert_eq!(Noise::from_u8(n as u8), n);
            let json = serde_json::to_string(&n).unwrap();
            assert_eq!(json, format!("\"{}\"", n.key()));
        }
        assert_eq!(Noise::default(), Noise::Ia);
        assert_eq!(Noise::from_u8(200), Noise::Ia);
        assert_eq!(Noise::by_key("krisp"), None);
    }

    #[test]
    fn the_model_reads_once_and_stays() {
        let a = load().unwrap();
        preload();
        let b = ready().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!broken());
    }
}
