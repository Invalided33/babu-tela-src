//! The app's name and version, shared by the desktop app, its installer and
//! the server. The version comes from the workspace Cargo.toml, its only
//! source.
//!
//! Also what the app and its installer have to agree on: where the install
//! is recorded, and the installer's command line for an update.

pub const NAME: &str = "Babu Tela";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The installed executable's name. The AppUserModelID of the notifications
/// is this same name (crates/app/src/notify.rs), as are the shortcuts'
/// targets.
pub const EXE: &str = "Babu Tela.exe";

/// The COM class Windows starts when someone clicks one of the installed
/// app's notifications after it left the screen, even with the app closed
/// (crates/app/src/notify.rs registers it; the uninstaller removes it).
pub const ACTIVATOR_CLSID: &str = "{AC272D24-9AA8-4704-A2AA-C1D8D5C594EE}";

/// The entry in "Apps" (per user): the installer writes it, with the
/// folder in `InstallLocation`, and the app reads it to know whether it is
/// the installed copy (only that one updates itself).
pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\BabuTela";

/// The installer's test mode: `BABUTELA_SETUP_TEST=<pasta>` moves every
/// folder it touches under that one and its registry keys under
/// HKCU\Software\BabuTela-teste (crates/setup/src/lib.rs).
pub const SETUP_TEST_ENV: &str = "BABUTELA_SETUP_TEST";

/// [`UNINSTALL_KEY`] in the installer's test mode.
pub const TEST_UNINSTALL_KEY: &str = r"Software\BabuTela-teste\Uninstall";

/// Whether the installer's test mode is on (see [`SETUP_TEST_ENV`]).
pub fn setup_test() -> bool {
    std::env::var_os(SETUP_TEST_ENV).is_some_and(|v| !v.is_empty())
}

/// Where the install is recorded, moved aside in the installer's test mode.
pub fn uninstall_key() -> &'static str {
    if setup_test() {
        TEST_UNINSTALL_KEY
    } else {
        UNINSTALL_KEY
    }
}

/// The named object the installer holds while it changes the app's files
/// (`Local\<this>`): an app that starts meanwhile leaves it alone, and the
/// installer opens it again when it is done.
pub fn setup_lock_name() -> String {
    if setup_test() {
        r"Local\com.prosa.app.teste-instalador.setup".into()
    } else {
        r"Local\com.prosa.app.setup".into()
    }
}

/// The emoji lists the pickers offer (crates/app/src/emoji.rs): the official
/// files, and the copies the server keeps of them (`<servidor>/emoji/<file>`,
/// sent to R2 by `cargo xtask emoji`). The app asks the server first, so a
/// person's PC talks only to it, and the official address only when the
/// server has no copy.
pub mod emoji {
    /// The CLDR release the Portuguese names come from: 49.0.0 is the first
    /// with the emoji of Unicode 18.0 (cldr-json's main branch was still on
    /// 48.2.3, without them, in October 2026).
    pub const CLDR: &str = "49.0.0";

    /// One of the files: its name (in the app's cache and on the server) and
    /// where it is published.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Source {
        pub file: &'static str,
        pub url: &'static str,
    }

    /// The list (UTS #51's emoji-test.txt, at the address that always has
    /// the newest version), then CLDR's names of the single emoji and of the
    /// sequences.
    pub const SOURCES: [Source; 3] = [
        Source {
            file: "emoji-test.txt",
            url: "https://www.unicode.org/Public/emoji/latest/emoji-test.txt",
        },
        Source {
            file: "annotations.json",
            url: "https://raw.githubusercontent.com/unicode-org/cldr-json/49.0.0/cldr-json/cldr-annotations-full/annotations/pt/annotations.json",
        },
        Source {
            file: "annotationsDerived.json",
            url: "https://raw.githubusercontent.com/unicode-org/cldr-json/49.0.0/cldr-json/cldr-annotations-derived-full/annotationsDerived/pt/annotations.json",
        },
    ];

    /// The server's copy of `file`.
    pub fn mirror(server: &str, file: &str) -> String {
        format!("{}/emoji/{file}", server.trim_end_matches('/'))
    }

    /// Whether `name` is one of the files the server keeps.
    pub fn is_file(name: &str) -> bool {
        SOURCES.iter().any(|s| s.file == name)
    }
}

/// What the app asks of the installer when it updates itself:
///
///   "Babu Tela Setup.exe" --update --wait-pid=<app> --relaunch [/S] [--minimized]
///
/// The installer waits for the app (which quits right after starting it)
/// to be gone, replaces it, and opens it again, whether the update worked
/// or not. Without /S it shows its progress in a small window. The
/// installer that runs is always the newer version's, so it knows every
/// flag the app may pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
    /// The app's process: its file is replaced once it has exited.
    pub wait_pid: Option<u32>,
    /// Open the app again afterwards.
    pub relaunch: bool,
    /// In the tray, the way it had been started.
    pub minimized: bool,
    /// Without a window (`/S`).
    pub silent: bool,
}

impl Update {
    /// The update the command line asks for, if it asks for one.
    pub fn from_args(args: &[String]) -> Option<Update> {
        if !args.iter().any(|a| a == "--update") {
            return None;
        }
        Some(Update {
            wait_pid: args
                .iter()
                .find_map(|a| a.strip_prefix("--wait-pid="))
                .and_then(|pid| pid.parse().ok()),
            relaunch: args.iter().any(|a| a == "--relaunch"),
            minimized: args.iter().any(|a| a == "--minimized"),
            silent: args.iter().any(|a| a.eq_ignore_ascii_case("/S")),
        })
    }

    /// The command line for it, as the installer reads it back.
    pub fn args(&self) -> Vec<String> {
        let mut out = vec!["--update".to_owned()];
        if let Some(pid) = self.wait_pid {
            out.push(format!("--wait-pid={pid}"));
        }
        if self.relaunch {
            out.push("--relaunch".into());
        }
        if self.minimized {
            out.push("--minimized".into());
        }
        if self.silent {
            out.push("/S".into());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_emoji_files_come_from_one_cldr_release() {
        let [list, names, derived] = emoji::SOURCES;
        assert!(list.url.starts_with("https://www.unicode.org/"));
        for s in [names, derived] {
            assert!(
                s.url.contains(&format!("/cldr-json/{}/", emoji::CLDR)),
                "{}",
                s.url
            );
        }
        assert_eq!(
            emoji::mirror("https://babutela.example/", "emoji-test.txt"),
            "https://babutela.example/emoji/emoji-test.txt"
        );
        assert!(emoji::is_file("annotationsDerived.json"));
        assert!(!emoji::is_file("../annotations.json") && !emoji::is_file(""));
    }

    #[test]
    fn the_update_command_line_reads_back() {
        for u in [
            Update {
                wait_pid: Some(4242),
                relaunch: true,
                minimized: false,
                silent: false,
            },
            Update {
                wait_pid: Some(7),
                relaunch: true,
                minimized: true,
                silent: true,
            },
            Update::default(),
        ] {
            assert_eq!(Update::from_args(&u.args()), Some(u.clone()));
        }
        let args: Vec<String> = ["/S".into()].into();
        assert_eq!(Update::from_args(&args), None);
        let odd: Vec<String> = ["--update".into(), "--wait-pid=abc".into(), "/s".into()].into();
        assert_eq!(
            Update::from_args(&odd),
            Some(Update {
                silent: true,
                ..Update::default()
            })
        );
    }
}
