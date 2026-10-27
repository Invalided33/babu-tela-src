//! The app around the interface: the profile folder, the settings, the
//! saved login and the window itself.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use serde::Serialize;
use serde_json::Value;

/// Where the chat server runs. Baked in at build time
/// (BABUTELA_SERVER_URL).
pub const SERVER_URL: &str = match option_env!("BABUTELA_SERVER_URL") {
    Some(url) => url,
    None => "https://babutela.babutela.workers.dev",
};

/// Saved as settings.json in the profile folder, with the same keys the
/// Electron, Go and Tauri versions used.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// The server this app talks to. Comes from the build, and is never
    /// written back: the earlier versions saved a `serverUrl` pointing at
    /// the server they ran on this PC, and honouring that would leave the
    /// app knocking forever on a door that no longer exists. Someone
    /// running their own copy of the server can still override it by
    /// putting `serverUrlOverride` in settings.json by hand.
    #[serde(skip)]
    pub server_url: String,
    pub notifications: bool,
    pub minimize_to_tray: bool,
    pub open_on_startup: bool,
    pub start_minimized: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            server_url: SERVER_URL.into(),
            notifications: true,
            minimize_to_tray: true,
            open_on_startup: false,
            start_minimized: false,
        }
    }
}

impl Settings {
    /// Overlays a saved settings.json; keys with the wrong type are ignored.
    fn merge(&mut self, saved: &Value) {
        let Value::Object(m) = saved else { return };
        let flag = |key: &str, dst: &mut bool| {
            if let Some(Value::Bool(v)) = m.get(key) {
                *dst = *v;
            }
        };
        if let Some(Value::String(v)) = m.get("serverUrlOverride") {
            self.server_url.clone_from(v);
        }
        flag("notifications", &mut self.notifications);
        flag("minimizeToTray", &mut self.minimize_to_tray);
        flag("openOnStartup", &mut self.open_on_startup);
        flag("startMinimized", &mut self.start_minimized);
        if self.server_url.trim().is_empty() {
            self.server_url = SERVER_URL.into();
        }
    }

    /// One of the four switches in the settings dialog, by its saved name.
    pub fn flag_mut(&mut self, key: &str) -> Option<&mut bool> {
        match key {
            "notifications" => Some(&mut self.notifications),
            "minimizeToTray" => Some(&mut self.minimize_to_tray),
            "openOnStartup" => Some(&mut self.open_on_startup),
            "startMinimized" => Some(&mut self.start_minimized),
            _ => None,
        }
    }
}

pub struct Shell {
    /// Profile folder: settings, the encrypted login, the picture cache.
    pub dir: PathBuf,
    settings: Mutex<Settings>,
    /// Held while an update is downloading (update.rs), so the checks at
    /// startup, every few hours and from the settings never download the
    /// same file at once.
    pub(crate) update_op: tokio::sync::Mutex<()>,
    hwnd: OnceLock<isize>,
}

/// `%APPDATA%\Prosa`, the folder the app has used since it was called Prosa
/// (another program on this machine already has a "Babu Tela" folder).
/// PROSA_USERDATA points it elsewhere, for tests.
fn profile_dir() -> PathBuf {
    match std::env::var_os("PROSA_USERDATA") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::var_os("APPDATA")
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join("Prosa"),
    }
}

impl Shell {
    pub fn new() -> Shell {
        let dir = profile_dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut settings = Settings::default();
        if let Ok(bytes) = std::fs::read(dir.join("settings.json"))
            && let Ok(saved) = serde_json::from_slice::<Value>(&bytes)
        {
            settings.merge(&saved);
        }
        Shell {
            dir,
            settings: Mutex::new(settings),
            update_op: tokio::sync::Mutex::new(()),
            hwnd: OnceLock::new(),
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn lock(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether this is the person's normal profile, not a test one
    /// (PROSA_USERDATA).
    pub fn own_profile(&self) -> bool {
        std::env::var_os("PROSA_USERDATA").is_none_or(|d| d.is_empty())
    }

    /// The name the single-instance lock goes by. A test profile
    /// (PROSA_USERDATA) never hands off to an installed copy that is open.
    pub fn instance_id(&self) -> String {
        if self.own_profile() {
            return "com.prosa.app".into();
        }
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(self.dir.to_string_lossy().as_bytes());
        format!("com.prosa.app.{}", hex::encode(&h[..6]))
    }

    pub fn server_url(&self) -> String {
        let url = self.lock().server_url.clone();
        url.trim().trim_end_matches('/').to_owned()
    }

    pub fn settings(&self) -> Settings {
        self.lock().clone()
    }

    /// Flips one switch, saves, and keeps "Iniciar com o Windows" in step.
    pub fn set_flag(&self, key: &str, value: bool) -> Settings {
        let out = {
            let mut s = self.lock();
            if let Some(slot) = s.flag_mut(key) {
                *slot = value;
            }
            let json = serde_json::to_string_pretty(&*s).unwrap_or_default();
            if let Err(e) = std::fs::write(self.path("settings.json"), json) {
                crate::logln!("salvando configurações: {e}");
            }
            s.clone()
        };
        if matches!(key, "openOnStartup" | "startMinimized")
            && let Err(e) = crate::winsys::set_login_item(out.open_on_startup, out.start_minimized)
        {
            crate::logln!("iniciar com o Windows: {e}");
        }
        out
    }

    pub fn start_hidden(&self) -> bool {
        self.lock().start_minimized || std::env::args().skip(1).any(|a| a == "--start-minimized")
    }

    // ------------------------------------------------------------ the login

    /// The saved session token, decrypted with the Windows user's key.
    pub fn load_session(&self) -> Option<String> {
        let enc = std::fs::read(self.path("session.bin")).ok()?;
        let plain = crate::winsys::unprotect(&enc).ok()?;
        let token = String::from_utf8(plain).ok()?;
        (!token.is_empty()).then_some(token)
    }

    /// Stores the token encrypted, so a copy of the profile folder on
    /// another account or machine is useless. `None` forgets it.
    pub fn save_session(&self, token: Option<&str>) -> bool {
        let file = self.path("session.bin");
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            let _ = std::fs::remove_file(file);
            return true;
        };
        // A smoke test signs in with a token of its own and must not
        // replace the real one.
        if std::env::var_os("PROSA_SMOKE_TOKEN").is_some() {
            return true;
        }
        match crate::winsys::protect(token.as_bytes()) {
            Ok(enc) => std::fs::write(file, enc).is_ok(),
            Err(_) => false,
        }
    }

    // ------------------------------------------------------------ the window

    pub fn set_hwnd(&self, hwnd: isize) {
        let _ = self.hwnd.set(hwnd);
    }

    pub fn hwnd(&self) -> Option<isize> {
        self.hwnd.get().copied()
    }

    /// The update downloaded and waiting for the next start, if there is
    /// one (update.rs).
    pub fn ready_update(&self) -> Option<crate::update::Pending> {
        crate::update::pending(&crate::update::dir(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_old_server_address_is_ignored() {
        let mut s = Settings::default();
        s.merge(&json!({
            "serverUrl": "http://localhost:3210",
            "notifications": false,
            "minimizeToTray": "sim",
            "startMinimized": true,
        }));
        assert_eq!(s.server_url, SERVER_URL);
        assert!(!s.notifications);
        // The wrong type leaves the default.
        assert!(s.minimize_to_tray);
        assert!(s.start_minimized);
        // The saved file never carries the address.
        let saved = serde_json::to_value(&s).unwrap();
        assert!(saved.get("serverUrl").is_none());
        assert!(saved.get("serverUrlOverride").is_none());
        assert_eq!(saved["notifications"], false);
    }

    #[test]
    fn an_override_can_point_elsewhere() {
        let mut s = Settings::default();
        s.merge(&json!({ "serverUrlOverride": "https://meu.servidor" }));
        assert_eq!(s.server_url, "https://meu.servidor");
        let mut blank = Settings::default();
        blank.merge(&json!({ "serverUrlOverride": "   " }));
        assert_eq!(blank.server_url, SERVER_URL);
    }

    #[test]
    fn switches_are_found_by_their_saved_names() {
        let mut s = Settings::default();
        for key in [
            "notifications",
            "minimizeToTray",
            "openOnStartup",
            "startMinimized",
        ] {
            assert!(s.flag_mut(key).is_some(), "{key}");
        }
        assert!(s.flag_mut("serverUrlOverride").is_none());
    }
}
