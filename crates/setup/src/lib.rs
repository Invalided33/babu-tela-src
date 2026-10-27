//! The parts of the installer that are not its window: the payload format
//! (shared with `cargo xtask release`, which packs it) and the names every
//! version has used, so a new installer lands on top of an old one.

pub mod payload;

/// What Windows calls the program, in "Apps" and on the shortcuts.
pub const PRODUCT: &str = babutela_buildinfo::NAME;

/// The executable's name. The AppUserModelID of the notifications is this
/// same name (see crates/app/src/notify.rs), as are the shortcuts' targets.
pub const EXE: &str = babutela_buildinfo::EXE;

/// The uninstaller, next to the app. Running it by this name uninstalls.
pub const UNINSTALLER: &str = "uninstall.exe";

/// Under %LOCALAPPDATA%. Not "Programs\Babu Tela": another program already
/// uses that folder on some machines.
pub const DIR: &str = r"Programs\BabuTela";

/// Per-user, so no admin prompt and the app can update itself silently.
/// The app reads it too (to know whether it is the installed copy), which
/// is why it lives in buildinfo.
pub const UNINSTALL_KEY: &str = babutela_buildinfo::UNINSTALL_KEY;

/// "Iniciar com o Windows" (crates/app/src/winsys.rs).
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The name the running app's single-instance objects go by
/// (crates/app/src/shell.rs): `Local\<id>.lock` is held while it runs and
/// setting `Local\<id>.quit` asks it to close.
pub const INSTANCE_ID: &str = "com.prosa.app";

/// The COM class Windows starts when someone clicks one of the app's
/// notifications after it left the screen (crates/app/src/notify.rs
/// registers it; the uninstaller removes it).
pub const ACTIVATOR_CLSID: &str = babutela_buildinfo::ACTIVATOR_CLSID;

/// Where to install when the person picks a folder: inside it, in a folder
/// of the app's own, the way NSIS did, so uninstalling never takes anything
/// else with it.
pub fn inside(chosen: &std::path::Path) -> std::path::PathBuf {
    let own = std::path::Path::new(DIR)
        .file_name()
        .unwrap_or_default()
        .to_owned();
    if chosen
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(&own))
    {
        chosen.to_path_buf()
    } else {
        chosen.join(own)
    }
}

/// Whether the command line asks for no window at all (`/S`, as NSIS took
/// it; the app's automatic update runs the installer like this).
pub fn silent(args: &[String]) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case("/S"))
}

/// For tests of the installer itself: `BABUTELA_SETUP_TEST=<pasta>` moves
/// every folder it touches under that one, its registry keys under
/// HKCU\Software\BabuTela-teste, and leaves any open copy of the real app
/// alone. Nothing of a real install is read or changed.
pub fn test_root() -> Option<std::path::PathBuf> {
    std::env::var_os(babutela_buildinfo::SETUP_TEST_ENV)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// The registry keys, moved aside in a test.
pub struct Keys {
    pub uninstall: String,
    pub run: String,
    pub classes: String,
}

pub fn keys() -> Keys {
    match test_root() {
        None => Keys {
            uninstall: UNINSTALL_KEY.into(),
            run: RUN_KEY.into(),
            classes: r"Software\Classes".into(),
        },
        Some(_) => Keys {
            uninstall: babutela_buildinfo::TEST_UNINSTALL_KEY.into(),
            run: r"Software\BabuTela-teste\Run".into(),
            classes: r"Software\BabuTela-teste\Classes".into(),
        },
    }
}

/// The running app's objects, or a name no real copy uses in a test.
pub fn instance_id() -> String {
    match test_root() {
        None => INSTANCE_ID.into(),
        Some(_) => format!("{INSTANCE_ID}.teste-instalador"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_chosen_folder_gets_a_folder_of_our_own() {
        assert_eq!(
            inside(Path::new(r"D:\Apps")),
            Path::new(r"D:\Apps\BabuTela")
        );
        assert_eq!(
            inside(Path::new(r"D:\Apps\babutela")),
            Path::new(r"D:\Apps\babutela")
        );
        assert_eq!(inside(Path::new(r"D:\")), Path::new(r"D:\BabuTela"));
    }

    #[test]
    fn silent_like_nsis() {
        assert!(silent(&["/S".into()]));
        assert!(silent(&["--x".into(), "/s".into()]));
        assert!(!silent(&["/SD".into()]));
        assert!(!silent(&[]));
    }
}
