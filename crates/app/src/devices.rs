//! Which microphone and which output the app uses: Windows' default ones,
//! or the ones chosen in Configurações → Voz e vídeo (prefs.json keeps
//! them). Calls, the cues and the attachments that play all follow it.
//!
//! A chosen device that is not there (unplugged) gives way to Windows'
//! default until it is back.

use std::str::FromStr;
use std::sync::{Mutex, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait};

/// A device as the settings show it and prefs.json keeps it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Device {
    /// cpal's id for it, which stays the same across runs and reboots.
    pub id: String,
    /// What Windows calls it ("Fone de ouvido (Realtek…)").
    pub name: String,
}

/// The microphones and the outputs this PC has now.
#[derive(Clone, Debug, Default)]
pub struct Devices {
    pub inputs: Vec<Device>,
    pub outputs: Vec<Device>,
}

/// The chosen ones, by id; None is Windows' default.
struct Chosen {
    input: Option<String>,
    output: Option<String>,
}

static CHOSEN: Mutex<Chosen> = Mutex::new(Chosen {
    input: None,
    output: None,
});

fn chosen() -> std::sync::MutexGuard<'static, Chosen> {
    CHOSEN.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn set_input(id: Option<String>) {
    chosen().input = id;
}

pub fn set_output(id: Option<String>) {
    chosen().output = id;
}

/// The chosen device (when it is there), then the default: what to try
/// opening, in that order.
fn to_try(
    host: &cpal::Host,
    id: Option<String>,
    default: Option<cpal::Device>,
) -> Vec<cpal::Device> {
    let picked = id
        .and_then(|id| cpal::DeviceId::from_str(&id).ok())
        .and_then(|id| host.device_by_id(&id));
    picked.into_iter().chain(default).collect()
}

/// The microphones to try opening, in order.
pub fn inputs(host: &cpal::Host) -> Vec<cpal::Device> {
    let id = chosen().input.clone();
    to_try(host, id, host.default_input_device())
}

/// The outputs to try opening, in order.
pub fn outputs(host: &cpal::Host) -> Vec<cpal::Device> {
    let id = chosen().output.clone();
    to_try(host, id, host.default_output_device())
}

/// The devices there are now. Asks Windows about each one (COM), so it
/// runs on a thread of its own, away from the window's.
pub fn list() -> Devices {
    let host = cpal::default_host();
    let named = |devices: Vec<cpal::Device>| -> Vec<Device> {
        let mut out: Vec<Device> = devices
            .into_iter()
            .filter_map(|d| {
                let id = d.id().ok()?.to_string();
                let name = d.description().ok()?.name().to_owned();
                Some(Device { id, name })
            })
            .collect();
        out.sort_by_key(|d| d.name.to_lowercase());
        out
    };
    Devices {
        inputs: named(
            host.input_devices()
                .map(Iterator::collect)
                .unwrap_or_default(),
        ),
        outputs: named(
            host.output_devices()
                .map(Iterator::collect)
                .unwrap_or_default(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_that_is_not_there_gives_way_to_the_default() {
        let host = cpal::default_host();
        let gone = Some("wasapi:{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}".into());
        let tried = to_try(&host, gone, None);
        assert!(tried.is_empty());
        // Nonsense in prefs.json is no device either.
        assert!(to_try(&host, Some("lixo".into()), None).is_empty());
    }

    /// Lists this PC's devices and opens the chosen one; needs sound
    /// devices.
    #[test]
    #[ignore = "precisa de dispositivos de som"]
    fn the_listed_devices_are_found_again_by_id() {
        let all = list();
        println!(
            "{} microfones, {} saídas",
            all.inputs.len(),
            all.outputs.len()
        );
        let host = cpal::default_host();
        for d in all.inputs.iter().chain(&all.outputs) {
            println!("  {}", d.name);
            let found = to_try(&host, Some(d.id.clone()), None);
            assert_eq!(found.len(), 1, "{d:?}");
        }
    }
}
