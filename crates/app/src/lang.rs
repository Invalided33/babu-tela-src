//! Translations. The app ships two catalogs, one per language, as JSON
//! embedded in the .exe (`assets/lang/`). They are parsed once at startup
//! and read by key.
//!
//! Portuguese is the source language: a key a catalog lacks falls back to
//! Portuguese, and then to the key itself, so a screen that has not been
//! translated yet shows Portuguese rather than blank text. That lets the
//! migration happen one screen at a time without ever breaking the app.
//!
//! Use the macros, never the lookup directly:
//!
//! - `t!("some.key")` for a plain string (returns `&'static str`).
//! - `tf!("some.key", name = value, ...)` for one with `{name}`
//!   placeholders (returns `String`).
//!
//! The current language is a global: egui is immediate mode, so changing
//! it with [`set`] shows on the next frame, with nothing to refresh.
//! Adding a third language later is one more JSON file and one enum arm.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// A language the app speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// Portuguese (the source language).
    Pt,
    /// English.
    En,
}

impl Lang {
    /// Every language, in the order the picker shows them.
    pub const ALL: [Lang; 2] = [Lang::Pt, Lang::En];

    /// The name shown in the picker, written in the language itself (an
    /// autonym, so it never needs translating).
    pub fn label(self) -> &'static str {
        match self {
            Lang::Pt => "Português",
            Lang::En => "English",
        }
    }

    /// The language a locale code names (`pt-BR` → Portuguese), by its
    /// first two letters; anything that is not Portuguese maps to English.
    fn from_locale(code: &str) -> Lang {
        if code.to_ascii_lowercase().starts_with("pt") {
            Lang::Pt
        } else {
            Lang::En
        }
    }
}

/// The current language, as `Lang as u8`.
static CURRENT: AtomicU8 = AtomicU8::new(Lang::Pt as u8);

/// The language the app is showing now.
pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        x if x == Lang::En as u8 => Lang::En,
        _ => Lang::Pt,
    }
}

/// Shows the app in `lang` from the next frame on.
pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// The language to start in before the person has chosen one: the one the
/// Windows account uses, Portuguese or (for anything else) English.
pub fn os_default() -> Lang {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
        // LOCALE_NAME_MAX_LENGTH is 85 UTF-16 units including the null.
        let mut buf = [0u16; 85];
        // SAFETY: GetUserDefaultLocaleName writes at most `buf.len()`
        // UTF-16 units (including the terminating null) into `buf`.
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        if n > 1 {
            let name = String::from_utf16_lossy(&buf[..(n as usize - 1)]);
            return Lang::from_locale(&name);
        }
    }
    Lang::Pt
}

/// Both catalogs, parsed once.
struct Catalog {
    pt: HashMap<String, String>,
    en: HashMap<String, String>,
}

impl Catalog {
    fn get() -> &'static Catalog {
        static CATALOG: OnceLock<Catalog> = OnceLock::new();
        CATALOG.get_or_init(|| Catalog {
            pt: parse(include_str!("../assets/lang/pt.json")),
            en: parse(include_str!("../assets/lang/en.json")),
        })
    }

    fn map(&self, lang: Lang) -> &HashMap<String, String> {
        match lang {
            Lang::Pt => &self.pt,
            Lang::En => &self.en,
        }
    }
}

fn parse(text: &str) -> HashMap<String, String> {
    serde_json::from_str(text).unwrap_or_else(|e| {
        crate::logln!("idioma: catálogo inválido: {e}");
        HashMap::new()
    })
}

/// The string for `key` in the current language, or Portuguese if the
/// current catalog lacks it. `None` only when neither catalog has it, in
/// which case the macro falls back to the key literal. The returned slice
/// lives as long as the program (the catalogs are a `OnceLock`).
pub fn lookup(key: &str) -> Option<&'static str> {
    let c = Catalog::get();
    c.map(current())
        .get(key)
        .or_else(|| c.pt.get(key))
        .map(String::as_str)
}

/// A plain translated string. `t!("key")` reads the current language and
/// falls back to the key literal if neither catalog has it, so it always
/// returns a `&'static str`.
#[macro_export]
macro_rules! t {
    ($key:literal) => {
        $crate::lang::lookup($key).unwrap_or($key)
    };
}

/// A translated string with `{name}` placeholders filled in:
/// `tf!("greeting", name = user)`. Each value is shown with `Display`.
/// Returns a `String`.
#[macro_export]
macro_rules! tf {
    ($key:literal $(, $name:ident = $val:expr)* $(,)?) => {{
        let mut s = ::std::string::String::from($crate::lang::lookup($key).unwrap_or($key));
        $(
            s = s.replace(
                ::std::concat!("{", ::std::stringify!($name), "}"),
                &$val.to_string(),
            );
        )*
        s
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogs() -> (HashMap<String, String>, HashMap<String, String>) {
        (
            parse(include_str!("../assets/lang/pt.json")),
            parse(include_str!("../assets/lang/en.json")),
        )
    }

    #[test]
    fn both_catalogs_parse_and_are_not_empty() {
        let (pt, en) = catalogs();
        assert!(!pt.is_empty(), "pt.json is empty or invalid");
        assert!(!en.is_empty(), "en.json is empty or invalid");
    }

    #[test]
    fn english_has_every_portuguese_key_and_no_extra() {
        let (pt, en) = catalogs();
        let missing: Vec<&String> = pt.keys().filter(|k| !en.contains_key(*k)).collect();
        let extra: Vec<&String> = en.keys().filter(|k| !pt.contains_key(*k)).collect();
        assert!(missing.is_empty(), "en.json is missing keys: {missing:?}");
        assert!(extra.is_empty(), "en.json has unknown keys: {extra:?}");
    }

    #[test]
    fn every_placeholder_matches_across_languages() {
        // A {name} present in one language must be present in the other,
        // or an interpolation would leave a stray {name} or drop a value.
        let (pt, en) = catalogs();
        for (key, p) in &pt {
            let Some(e) = en.get(key) else { continue };
            assert_eq!(
                placeholders(p),
                placeholders(e),
                "placeholders differ for key {key:?}"
            );
        }
    }

    fn placeholders(s: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut rest = s;
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            if let Some(close) = rest.find('}') {
                out.insert(rest[..close].to_owned());
                rest = &rest[close + 1..];
            } else {
                break;
            }
        }
        out
    }

    #[test]
    fn tf_fills_placeholders() {
        // Uses a key we know exists; if the key text changes, the count of
        // substituted braces is what matters, not the surrounding words.
        super::set(Lang::Pt);
        let s = tf!("settings.version", version = "9.9.9");
        assert!(s.contains("9.9.9"), "version not interpolated: {s}");
        assert!(!s.contains("{version}"), "placeholder left behind: {s}");
    }

    #[test]
    fn switching_language_changes_the_text() {
        super::set(Lang::Pt);
        let pt = t!("common.close");
        super::set(Lang::En);
        let en = t!("common.close");
        super::set(Lang::Pt);
        assert_ne!(pt, en, "close button did not translate");
    }
}
