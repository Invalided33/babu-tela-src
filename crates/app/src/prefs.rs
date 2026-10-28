//! What the old page kept in localStorage: the last screen, the last channel
//! of each server, the video quality and which sounds are off. Lives in
//! prefs.json in the profile folder.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::View;
use crate::devices::Device;
use crate::voice::Noise;
use crate::voice::audio::Gains;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Prefs {
    pub view: Option<View>,
    /// Last channel per server; the key is the server id as text, so the
    /// file stays plain JSON.
    pub last: HashMap<String, i64>,
    pub quality: Option<String>,
    pub sounds_off: Vec<String>,
    /// How loud each person plays here (right click in a call), by user id
    /// as text; only those changed from the defaults.
    pub people: HashMap<String, PersonAudio>,
    /// Whether the PC's sound goes with a screen share (on unless turned
    /// off in the share picker).
    pub share_audio: Option<bool>,
    /// The microphone and the output chosen in the settings (none:
    /// Windows' default).
    pub input_device: Option<Device>,
    pub output_device: Option<Device>,
    /// The noise suppression chosen (none: the network).
    pub noise: Option<Noise>,
    /// The emoji picked last, newest first: the picker's first rows.
    pub emoji_recent: Vec<String>,
    /// The skin tone the emoji picker gives, 1 (light) to 5 (dark); 0 for
    /// none, the yellow.
    pub emoji_tone: u8,
    /// Calls and files sent directly go only through the relay, so the
    /// others see its address instead of this PC's (voice/rtc.rs
    /// `set_hide_ip`).
    pub hide_ip: bool,
    /// "Digitando…" is not sent: the server does not learn when I write.
    pub no_typing: bool,
    /// Everything goes through Tor, inside the app (tor.rs); read at the
    /// start.
    pub tor: bool,
    /// The language chosen in Configurações (none: the Windows account's,
    /// set the first time the app runs).
    pub lang: Option<crate::lang::Lang>,
}

/// How loud one person plays on this PC: their voice and the sound of
/// their screen share, each from 0 to 2 (200 %) and silenced or not.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PersonAudio {
    pub voice: f32,
    pub voice_muted: bool,
    pub screen: f32,
    pub screen_muted: bool,
}

impl Default for PersonAudio {
    fn default() -> PersonAudio {
        PersonAudio {
            voice: 1.0,
            voice_muted: false,
            screen: 1.0,
            screen_muted: false,
        }
    }
}

impl PersonAudio {
    /// What the speaker applies.
    pub fn gains(&self) -> Gains {
        let level = |muted: bool, v: f32| if muted { 0.0 } else { v.clamp(0.0, 2.0) };
        Gains {
            voice: level(self.voice_muted, self.voice),
            screen: level(self.screen_muted, self.screen),
        }
    }
}

impl Prefs {
    /// Reads the file; a missing or damaged one gives the defaults.
    pub fn load(path: &Path) -> Prefs {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        let Ok(text) = serde_json::to_vec_pretty(self) else {
            return;
        };
        // Written next to the old one and moved over it, so a crash mid-way
        // never leaves half a file.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("babutela-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.json");
        assert!(Prefs::load(&path).view.is_none());

        let p = Prefs {
            view: Some(View::Server {
                server: 2,
                channel: Some(5),
            }),
            last: [("2".to_owned(), 5)].into_iter().collect(),
            quality: Some("720p30".into()),
            sounds_off: vec!["join".into()],
            people: [(
                "7".to_owned(),
                PersonAudio {
                    voice: 1.5,
                    screen_muted: true,
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            share_audio: Some(false),
            input_device: Some(Device {
                id: "wasapi:{0.0.1.00000000}.{abc}".into(),
                name: "Microfone (USB)".into(),
            }),
            output_device: None,
            noise: Some(Noise::Padrao),
            emoji_recent: vec!["\u{1F44D}\u{1F3FD}".into(), "\u{1F600}".into()],
            emoji_tone: 3,
            hide_ip: false,
            no_typing: false,
            tor: false,
            lang: Some(crate::lang::Lang::En),
        };
        p.save(&path);

        let back = Prefs::load(&path);
        assert_eq!(back.view, p.view);
        assert_eq!(back.last.get("2"), Some(&5));
        assert_eq!(back.quality.as_deref(), Some("720p30"));
        assert_eq!(back.sounds_off, ["join"]);
        assert_eq!(back.people.get("7"), p.people.get("7"));
        assert_eq!(back.share_audio, Some(false));
        assert_eq!(back.lang, Some(crate::lang::Lang::En));
        assert_eq!(back.input_device, p.input_device);
        assert_eq!(back.output_device, None);
        assert_eq!(back.noise, Some(Noise::Padrao));
        assert_eq!(back.emoji_recent, p.emoji_recent);
        assert_eq!(back.emoji_tone, 3);
        // A file from before these knows nothing of them.
        std::fs::write(&path, br#"{"quality": "720p30"}"#).unwrap();
        let old = Prefs::load(&path);
        assert!(old.people.is_empty() && old.share_audio.is_none() && old.noise.is_none());
        assert!(old.emoji_recent.is_empty() && old.emoji_tone == 0);

        std::fs::write(&path, b"{ lixo").unwrap();
        assert!(Prefs::load(&path).view.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_silenced_person_plays_at_zero_and_volumes_stay_in_range() {
        let p = PersonAudio {
            voice: 3.0,
            screen: 0.4,
            screen_muted: true,
            ..Default::default()
        };
        assert_eq!(
            p.gains(),
            Gains {
                voice: 2.0,
                screen: 0.0
            }
        );
        assert_eq!(PersonAudio::default().gains(), Gains::default());
    }
}
