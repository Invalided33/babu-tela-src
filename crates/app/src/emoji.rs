//! The emoji the pickers offer: every one of the Unicode standard, read from
//! the official lists instead of a handful written down in the app.
//!
//! - Which emoji there are, in which groups, in which order and with which
//!   skin tones: emoji-test.txt of Unicode Technical Standard #51, from the
//!   Unicode Consortium, at the address that always has the newest version
//!   ([`LIST_URL`]). It is the list emoji keyboards follow, Google's
//!   included: Google's own picker data (github.com/googlefonts/
//!   emoji-metadata) is made from it, and was a version behind it in
//!   October 2026 (17.0 against 18.0).
//! - Their names and the words to find them by, in Portuguese: CLDR, the
//!   Unicode Consortium's locale data, in its JSON edition
//!   (github.com/unicode-org/cldr-json, "pt", which is Brazil's). The
//!   derived file names the sequences (flags, families, skin tones).
//!   Without them the picker still works, with the English names of the
//!   list.
//! - Where from: the copies our server keeps (`<servidor>/emoji/`, which
//!   `cargo xtask emoji` fills from the official addresses), so a person's
//!   PC talks only to it; the official addresses only when the server has
//!   none (babutela_buildinfo::emoji).
//! - Downloaded the first time, they are kept in the profile's cache and
//!   asked again (only whether they changed) at most once a week; without
//!   the internet, the copy kept is used however old it is. A file that is
//!   not what its name says (a proxy's page, half a file) is never kept.
//! - Only what this PC draws: an emoji newer than Windows' emoji font and
//!   than the one the app carries would come out as pieces or as a box, so
//!   each one is laid out the way a message lays it out (emoji_font.rs) and
//!   kept only if it comes out whole, in colour.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::emoji_font::{EmojiFonts, Face, Route};

/// Points the app at another copy of the three files (`<base>/emoji-test.txt`,
/// `<base>/annotations.json`, `<base>/annotationsDerived.json`), for tests.
pub const URL_ENV: &str = "BABUTELA_EMOJI_URL";

/// The licence of Unicode's files (www.unicode.org/license.txt), whose
/// notice has to go with every copy of them or in its documentation: the
/// server keeps copies, and the CLDR ones carry no notice of their own. The
/// app shows it (Configurações → Licenças).
pub const UNICODE_LICENSE: &str = include_str!("../assets/UNICODE-LICENSE.txt");

/// How long a kept copy is used before asking whether it changed.
const REFRESH: Duration = Duration::from_secs(7 * 24 * 3600);

/// How long one download may take.
const TIMEOUT: Duration = Duration::from_secs(60);

/// How many recent emoji the picker keeps: its first three rows.
pub const RECENT: usize = 27;

/// The tones of the list, light to dark, as its names say them.
const TONES: [&str; 5] = [
    "light skin tone",
    "medium-light skin tone",
    "medium skin tone",
    "medium-dark skin tone",
    "dark skin tone",
];

/// The skin tone modifiers, light to dark (U+1F3FB to U+1F3FF).
pub const MODIFIERS: [&str; 5] = [
    "\u{1F3FB}",
    "\u{1F3FC}",
    "\u{1F3FD}",
    "\u{1F3FE}",
    "\u{1F3FF}",
];

/// The tones in Portuguese, should CLDR not be there.
pub const TONE_NAMES: [&str; 5] = [
    "pele clara",
    "pele morena clara",
    "pele morena",
    "pele morena escura",
    "pele escura",
];

/// The groups of the list in Portuguese, with the emoji on their tab. A
/// group a newer list brings keeps its English name and its first emoji.
const GROUPS: &[(&str, &str, &str)] = &[
    ("Smileys & Emotion", "Smileys e emoções", "\u{1F600}"),
    ("People & Body", "Pessoas e corpo", "\u{1F44B}"),
    ("Animals & Nature", "Animais e natureza", "\u{1F43B}"),
    ("Food & Drink", "Comidas e bebidas", "\u{1F354}"),
    ("Travel & Places", "Viagens e lugares", "\u{1F697}"),
    ("Activities", "Atividades", "\u{26BD}"),
    ("Objects", "Objetos", "\u{1F4A1}"),
    ("Symbols", "Símbolos", "\u{1F523}"),
    ("Flags", "Bandeiras", "\u{1F3C1}"),
];

// ---------------------------------------------------------------- the catalog

/// One emoji of the picker: the one without a skin tone, and the same in
/// each of the five tones when it has them.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub text: String,
    /// In Portuguese when CLDR has it, otherwise the list's English name.
    pub name: String,
    /// Light to dark; a tone this PC cannot draw is the emoji without one.
    pub tones: Option<Box<[String; 5]>>,
    /// For two people (a handshake, a couple): a tone for each, the first
    /// person's by row and the second's by column, light to dark. The same
    /// tone twice is the emoji in that tone.
    pub pairs: Option<Box<[[String; 5]; 5]>>,
    /// The words it is found by, folded (`fold`), each after a space.
    words: String,
    /// The words of its names only, which come first in a search.
    name_words: String,
}

impl Entry {
    /// As shown with `tone`: 1 (light) to 5 (dark), 0 for none.
    pub fn with_tone(&self, tone: u8) -> &str {
        match (&self.tones, tone) {
            (Some(tones), 1..=5) => &tones[usize::from(tone - 1)],
            _ => &self.text,
        }
    }

    /// With a tone for each person, 1 (light) to 5 (dark).
    pub fn pair(&self, first: u8, second: u8) -> Option<&str> {
        let row = self
            .pairs
            .as_ref()?
            .get(usize::from(first.checked_sub(1)?))?;
        row.get(usize::from(second.checked_sub(1)?))
            .map(String::as_str)
    }

    /// Which tones `text` gives each person, if it is one of the pairs
    /// (two different tones).
    pub fn pair_of(&self, text: &str) -> Option<(u8, u8)> {
        let pairs = self.pairs.as_ref()?;
        (0..5u8)
            .flat_map(|a| (0..5u8).map(move |b| (a, b)))
            .find(|&(a, b)| a != b && pairs[usize::from(a)][usize::from(b)] == text)
            .map(|(a, b)| (a + 1, b + 1))
    }
}

/// A group of the list, one tab of the picker.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// As the list names it ("Smileys & Emotion").
    pub key: String,
    /// In Portuguese.
    pub label: String,
    /// The emoji on its tab.
    pub icon: String,
    /// Its entries in [`Catalog::entries`].
    pub range: Range<usize>,
}

#[derive(Debug, Default)]
pub struct Catalog {
    /// The Unicode Emoji version of the list ("18.0").
    pub version: String,
    pub groups: Vec<Group>,
    pub entries: Vec<Entry>,
    /// The tones in Portuguese, light to dark.
    pub tone_names: [String; 5],
    /// Whether the names are CLDR's Portuguese ones.
    pub portuguese: bool,
    /// Emoji of the list (tones included) this PC cannot draw.
    pub hidden: usize,
    /// Every emoji on offer, its tones too, to its entry.
    index: HashMap<String, usize>,
}

impl Catalog {
    /// The entry an emoji is, with or without a tone (or its emoji
    /// presentation selector).
    pub fn entry_of(&self, text: &str) -> Option<&Entry> {
        let i = self
            .index
            .get(text)
            .or_else(|| self.index.get(&text.replace('\u{fe0f}', "")))?;
        self.entries.get(*i)
    }

    /// What to call an emoji, its tones included ("polegar para cima: pele
    /// morena", "aperto de mãos: pele clara e pele escura", as CLDR says
    /// them).
    pub fn name_of(&self, text: &str) -> Option<String> {
        let e = self.entry_of(text)?;
        if let Some((a, b)) = e.pair_of(text) {
            let tone = |t: u8| &self.tone_names[usize::from(t - 1)];
            return Some(format!("{}: {} e {}", e.name, tone(a), tone(b)));
        }
        let tone = e
            .tones
            .iter()
            .flat_map(|t| t.iter())
            .position(|t| t == text && t != &e.text);
        Some(match tone {
            Some(t) => format!("{}: {}", e.name, self.tone_names[t]),
            None => e.name.clone(),
        })
    }

    /// The entries a search finds, those whose names have the words first,
    /// then those with them among their keywords, each in the list's order.
    /// Every word must start a word of the emoji: "cora" finds "coração".
    pub fn search(&self, query: &str) -> Vec<usize> {
        let needles: Vec<String> = words_of(query).map(|w| format!(" {w}")).collect();
        if needles.is_empty() {
            return Vec::new();
        }
        let hit = |hay: &str| needles.iter().all(|n| hay.contains(n.as_str()));
        let (mut named, mut tagged) = (Vec::new(), Vec::new());
        for (i, e) in self.entries.iter().enumerate() {
            if hit(&e.name_words) {
                named.push(i);
            } else if hit(&e.words) {
                tagged.push(i);
            }
        }
        named.extend(tagged);
        named
    }
}

/// Lower case and without accents, for searching: "Coração" meets
/// "coracao". The Latin letters with the marks Portuguese and the
/// languages near it use; anything else stays as it is.
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

/// The folded words of a text.
fn words_of(s: &str) -> impl Iterator<Item = String> {
    fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
}

/// The words, each after a space, for `Catalog::search`.
fn haystack<'a>(texts: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    let mut seen = HashSet::new();
    for w in texts.flat_map(words_of) {
        if seen.insert(w.clone()) {
            out.push(' ');
            out.push_str(&w);
        }
    }
    out
}

// ---------------------------------------------------------------- reading the files

/// What emoji-test.txt lists, before the names and the drawing.
#[derive(Debug, Default, PartialEq)]
struct Listed {
    version: String,
    /// The list's groups in order (its "Component" one left out), with
    /// their emoji.
    groups: Vec<(String, Vec<Base>)>,
}

/// An emoji of the list, with the same emoji in each skin tone.
#[derive(Debug, Default, PartialEq)]
struct Base {
    text: String,
    /// The list's English name.
    name: String,
    tones: [Option<String>; 5],
    /// Two people of different tones: the first person's tone by row, the
    /// second's by column (the same tone twice is in `tones`).
    pairs: [[Option<String>; 5]; 5],
}

/// What the name of an emoji of the list says about its skin tone.
#[derive(Debug, PartialEq)]
enum Tone {
    /// None: an emoji of its own.
    None,
    /// One, given to the emoji named so.
    One(usize, String),
    /// One for each of two people (a handshake, a couple), the first
    /// person's first.
    Two(usize, usize, String),
    /// More, which no emoji has.
    Many,
}

/// "waving hand: medium skin tone" is "waving hand" in tone 3, "man: light
/// skin tone, red hair" is "man: red hair" in tone 1, and "handshake: light
/// skin tone, dark skin tone" is "handshake" in tones 1 and 5.
fn tone_of(name: &str) -> Tone {
    let Some((head, tail)) = name.split_once(": ") else {
        return Tone::None;
    };
    let parts: Vec<&str> = tail.split(", ").collect();
    let tones: Vec<(usize, usize)> = parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| TONES.iter().position(|t| t == p).map(|t| (i, t)))
        .collect();
    let rest: Vec<&str> = parts
        .iter()
        .enumerate()
        .filter(|(i, _)| tones.iter().all(|(at, _)| at != i))
        .map(|(_, p)| *p)
        .collect();
    let base = if rest.is_empty() {
        head.to_owned()
    } else {
        format!("{head}: {}", rest.join(", "))
    };
    match tones[..] {
        [] => Tone::None,
        [(_, tone)] => Tone::One(tone, base),
        [(_, a), (_, b)] => Tone::Two(a, b, base),
        _ => Tone::Many,
    }
}

/// The characters of a list line's code points ("1F44B 1F3FD").
fn characters(points: &str) -> Option<String> {
    points
        .split_whitespace()
        .map(|p| u32::from_str_radix(p, 16).ok().and_then(char::from_u32))
        .collect()
}

/// Reads emoji-test.txt: its fully-qualified emoji (the ones keyboards
/// offer), group by group, each tone given to the emoji it is a tone of.
fn parse_list(text: &str) -> Listed {
    let mut out = Listed::default();
    let mut named: HashMap<String, (usize, usize)> = HashMap::new();
    let mut group: Option<usize> = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("# Version:") {
            out.version = v.trim().to_owned();
            continue;
        }
        if let Some(g) = line.strip_prefix("# group:") {
            let g = g.trim();
            // The tone and hair pieces, which are never picked alone.
            group = (g != "Component").then(|| {
                out.groups.push((g.to_owned(), Vec::new()));
                out.groups.len() - 1
            });
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Some(g) = group else { continue };
        let Some((points, rest)) = line.split_once(';') else {
            continue;
        };
        let Some((status, comment)) = rest.split_once('#') else {
            continue;
        };
        if status.trim() != "fully-qualified" {
            continue;
        }
        let Some(text) = characters(points) else {
            continue;
        };
        // "😀 E1.0 grinning face": the emoji, its version, its name.
        let mut parts = comment.trim().splitn(3, ' ');
        let (Some(_), Some(_), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim().to_owned();
        match tone_of(&name) {
            Tone::None => {
                named.insert(name.clone(), (g, out.groups[g].1.len()));
                out.groups[g].1.push(Base {
                    text,
                    name,
                    ..Base::default()
                });
            }
            Tone::One(tone, base) => {
                if let Some(&(bg, bi)) = named.get(&base) {
                    out.groups[bg].1[bi].tones[tone] = Some(text);
                }
            }
            // "kiss: person, person, light skin tone, dark skin tone" is
            // the kiss, 💏, whose tones are "kiss: light skin tone".
            Tone::Two(a, b, base) if a != b => {
                let found = named
                    .get(&base)
                    .or_else(|| base.split_once(": ").and_then(|(head, _)| named.get(head)));
                if let Some(&(bg, bi)) = found {
                    out.groups[bg].1[bi].pairs[a][b] = Some(text);
                }
            }
            Tone::Two(..) | Tone::Many => {}
        }
    }
    out
}

/// CLDR's words for one emoji: its keywords and the name it is read as.
#[derive(Debug, Default, Deserialize)]
struct Words {
    #[serde(default)]
    default: Vec<String>,
    #[serde(default)]
    tts: Vec<String>,
}

#[derive(Deserialize)]
struct CldrFile {
    #[serde(alias = "annotationsDerived")]
    annotations: CldrBody,
}

#[derive(Deserialize)]
struct CldrBody {
    annotations: HashMap<String, Words>,
}

/// Reads one of CLDR's annotation files (plain or derived), keyed by the
/// emoji without their presentation selectors, as CLDR writes them.
fn parse_names(json: &str) -> Option<HashMap<String, Words>> {
    serde_json::from_str::<CldrFile>(json)
        .ok()
        .map(|f| f.annotations.annotations)
}

/// Puts it all together. `draws` says whether this PC draws an emoji (see
/// [`Painter`]); what it does not is left out.
fn build(
    listed: Listed,
    names: &[HashMap<String, Words>],
    mut draws: impl FnMut(&str) -> bool,
) -> Catalog {
    let words_of = |text: &str| {
        let key = text.replace('\u{fe0f}', "");
        names.iter().find_map(|m| m.get(&key))
    };
    let mut c = Catalog {
        version: listed.version,
        portuguese: !names.is_empty(),
        tone_names: std::array::from_fn(|t| {
            words_of(MODIFIERS[t])
                .and_then(|w| w.tts.first().cloned())
                .unwrap_or_else(|| TONE_NAMES[t].to_owned())
        }),
        ..Catalog::default()
    };
    for (key, bases) in listed.groups {
        let start = c.entries.len();
        for b in bases {
            let tone_count = b.tones.iter().flatten().count();
            let pair_count = b.pairs.iter().flatten().flatten().count();
            if !draws(&b.text) {
                c.hidden += 1 + tone_count + pair_count;
                continue;
            }
            let mut any = false;
            let tones: [String; 5] = std::array::from_fn(|t| match &b.tones[t] {
                Some(v) if draws(v) => {
                    any = true;
                    v.clone()
                }
                _ => b.text.clone(),
            });
            c.hidden += tone_count - tones.iter().filter(|t| **t != b.text).count();
            // A tone for each person only when every pair is there and
            // drawn, with every tone of both: a grid with holes would
            // offer a choice it then cannot give.
            let pairs = (pair_count == 20 && tones.iter().all(|t| *t != b.text))
                .then(|| {
                    let mut grid: [[String; 5]; 5] = Default::default();
                    for (a, row) in grid.iter_mut().enumerate() {
                        for (z, cell) in row.iter_mut().enumerate() {
                            *cell = if a == z {
                                tones[a].clone()
                            } else {
                                b.pairs[a][z].clone()?
                            };
                        }
                    }
                    grid.iter()
                        .flatten()
                        .all(|t| draws(t))
                        .then(|| Box::new(grid))
                })
                .flatten();
            if pairs.is_none() {
                c.hidden += pair_count;
            }
            let cldr = words_of(&b.text);
            let name = cldr
                .and_then(|w| w.tts.first().cloned())
                .unwrap_or_else(|| b.name.clone());
            let keywords = cldr.map_or(&[][..], |w| &w.default[..]);
            let i = c.entries.len();
            c.index.insert(b.text.clone(), i);
            if any {
                for t in &tones {
                    c.index.entry(t.clone()).or_insert(i);
                }
            }
            for t in pairs.iter().flat_map(|p| p.iter().flatten()) {
                c.index.entry(t.clone()).or_insert(i);
            }
            c.entries.push(Entry {
                words: haystack(
                    [name.as_str(), b.name.as_str()]
                        .into_iter()
                        .chain(keywords.iter().map(String::as_str)),
                ),
                name_words: haystack([name.as_str(), b.name.as_str()].into_iter()),
                text: b.text,
                name,
                tones: any.then(|| Box::new(tones)),
                pairs,
            });
        }
        if c.entries.len() == start {
            continue;
        }
        let (label, icon) = match GROUPS.iter().find(|(k, ..)| *k == key) {
            Some((_, label, icon)) if draws(icon) => ((*label).to_owned(), (*icon).to_owned()),
            Some((_, label, _)) => ((*label).to_owned(), c.entries[start].text.clone()),
            None => (key.clone(), c.entries[start].text.clone()),
        };
        c.groups.push(Group {
            key,
            label,
            icon,
            range: start..c.entries.len(),
        });
    }
    c
}

// ---------------------------------------------------------------- what this PC draws

/// Says whether an emoji comes out whole, in colour, the way a message
/// shows it (emoji_font.rs): from the app's fonts, or from an emoji font on
/// its own. Thread-safe, so it runs away from the window's thread.
pub struct Painter {
    fonts: Arc<EmojiFonts>,
}

impl Painter {
    /// With fonts of its own.
    #[cfg(test)]
    pub fn new() -> Painter {
        Painter {
            fonts: Arc::new(EmojiFonts::new()),
        }
    }

    /// Through the window's emoji fonts, which keep what it finds: the
    /// picker then opens without finding it out in a frame.
    fn window() -> Painter {
        Painter {
            fonts: crate::emoji_font::shared(),
        }
    }

    /// How one emoji is drawn; None for more than one.
    pub fn route(&self, text: &str) -> Option<Route> {
        use unicode_segmentation::UnicodeSegmentation as _;
        let mut clusters = text.graphemes(true);
        if clusters.next().is_none() || clusters.next().is_some() {
            return None;
        }
        Some(self.fonts.route(&egui::FontFamily::Proportional, text))
    }

    #[cfg(test)]
    pub fn draws(&self, text: &str) -> bool {
        matches!(self.route(text), Some(Route::Chain | Route::Own(_)))
    }
}

// ---------------------------------------------------------------- downloading

/// One file: its name in the cache, and where to ask for it, in order.
struct Source {
    file: &'static str,
    urls: Vec<String>,
}

/// The server's copy of each file first, then the official one
/// (babutela_buildinfo::emoji). [`URL_ENV`] replaces both, for tests.
fn sources(server: &str) -> [Source; 3] {
    let base = std::env::var(URL_ENV)
        .ok()
        .map(|b| b.trim_end_matches('/').to_owned())
        .filter(|b| !b.is_empty());
    babutela_buildinfo::emoji::SOURCES.map(|s| Source {
        file: s.file,
        urls: match &base {
            Some(b) => vec![format!("{b}/{}", s.file)],
            None if server.is_empty() => vec![s.url.to_owned()],
            None => vec![
                babutela_buildinfo::emoji::mirror(server, s.file),
                s.url.to_owned(),
            ],
        },
    })
}

/// Whether a file is what its name says: the list ends the way Unicode's
/// files end, the names are JSON with CLDR's annotations. Otherwise a
/// proxy's page, an error answered as 200 or half a file would be kept as
/// the list for a week.
fn looks_right(file: &str, text: &str) -> bool {
    if file.ends_with(".json") {
        text.contains("\"annotations\"")
            && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
    } else {
        text.contains("\n# Version:")
            && text.contains("\n# group:")
            && text.trim_end().ends_with("EOF")
    }
}

/// What is known about the copies kept (`fontes.json` in the cache).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    files: HashMap<String, KeptFile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct KeptFile {
    url: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    modified: Option<String>,
    /// When the server last said it was this one (ms since 1970).
    #[serde(default)]
    checked: i64,
}

const KEPT: &str = "fontes.json";

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Writes a file in one step (a crash halfway leaves the old one).
fn write_whole(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let fresh = path.with_extension("novo");
    std::fs::write(&fresh, bytes)?;
    std::fs::rename(&fresh, path)
}

/// What one place said about a file.
enum Answer {
    /// The copy kept from there is still it (304).
    Same,
    New {
        text: String,
        etag: Option<String>,
        modified: Option<String>,
    },
}

/// One request, and its answer only if it is the file asked for.
async fn ask(req: reqwest::RequestBuilder, url: &str, file: &str) -> Result<Answer, String> {
    let res = req.send().await.map_err(|e| format!("{url}: {e}"))?;
    let status = res.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Answer::Same);
    }
    if !status.is_success() {
        return Err(format!("{url} respondeu {}", status.as_u16()));
    }
    let header = |name: reqwest::header::HeaderName| {
        res.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let (etag, modified) = (
        header(reqwest::header::ETAG),
        header(reqwest::header::LAST_MODIFIED),
    );
    let text = res.text().await.map_err(|e| format!("{url}: {e}"))?;
    if !looks_right(file, &text) {
        return Err(format!("{url} não mandou o {file} de verdade"));
    }
    Ok(Answer::New {
        text,
        etag,
        modified,
    })
}

/// One file: the copy kept while it is fresh, otherwise asked of each place
/// in turn (only whether it changed, where the copy came from). Without an
/// answer, the copy kept, however old. Also what to write down about it
/// now, if anything changed.
async fn fetch(
    http: &reqwest::Client,
    dir: &Path,
    old: Option<KeptFile>,
    src: &Source,
) -> (Result<String, String>, Option<KeptFile>) {
    let path = dir.join(src.file);
    // A damaged copy is no copy: asked about, it would be "not modified".
    let have = std::fs::read_to_string(&path)
        .ok()
        .filter(|t| looks_right(src.file, t));
    if let (Some(k), Some(text)) = (&old, &have)
        && src.urls.contains(&k.url)
        && now_ms() - k.checked < REFRESH.as_millis() as i64
    {
        return (Ok(text.clone()), None);
    }
    let mut why = format!("{}: nenhum endereço", src.file);
    for url in &src.urls {
        let same = old.as_ref().filter(|k| &k.url == url && have.is_some());
        let mut req = http.get(url).timeout(TIMEOUT);
        if let Some(k) = same {
            if let Some(etag) = &k.etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            if let Some(modified) = &k.modified {
                req = req.header(reqwest::header::IF_MODIFIED_SINCE, modified);
            }
        }
        match ask(req, url, src.file).await {
            Ok(Answer::Same) => match (same, &have) {
                (Some(k), Some(text)) => {
                    let record = KeptFile {
                        checked: now_ms(),
                        ..k.clone()
                    };
                    return (Ok(text.clone()), Some(record));
                }
                _ => why = format!("{url} respondeu 304 sem uma cópia daqui"),
            },
            Ok(Answer::New {
                text,
                etag,
                modified,
            }) => {
                if let Err(e) = write_whole(&path, text.as_bytes()) {
                    crate::logln!("emoji: guardando {}: {e}", src.file);
                }
                let record = KeptFile {
                    url: url.clone(),
                    etag,
                    modified,
                    checked: now_ms(),
                };
                return (Ok(text), Some(record));
            }
            Err(e) => why = e,
        }
        crate::logln!("emoji: {why}");
    }
    match have {
        Some(text) => {
            crate::logln!("emoji: {}: usando a cópia guardada", src.file);
            (Ok(text), None)
        }
        None => (Err(why), None),
    }
}

/// What this PC could not draw, and what it draws from an emoji font on its
/// own (emoji_font.rs), written down (`visiveis.json`) for the list and the
/// fonts it was found with: laying every emoji out again at each start
/// would cost seconds for the same answer. Everything else of the list is
/// drawn by the app's fonts.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Drawn {
    key: String,
    hidden: Vec<String>,
    #[serde(default)]
    own: BTreeMap<Face, Vec<String>>,
}

const DRAWN: &str = "visiveis.json";

/// What the answer depends on: this app's version and the way it draws
/// emoji (the fonts it carries, emoji_font::METHOD), the list itself and
/// the font files Windows has.
fn drawing_key(list: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(babutela_buildinfo::VERSION.as_bytes());
    h.update(crate::emoji_font::METHOD.as_bytes());
    h.update(Sha256::digest(list.as_bytes()));
    for (path, len, changed) in crate::theme::font_stamp() {
        h.update(path.as_bytes());
        h.update(len.to_le_bytes());
        h.update(changed.to_le_bytes());
    }
    hex::encode(h.finalize())
}

/// The catalog, with what this PC draws as written down when it still
/// holds, otherwise found out (and written down).
fn catalog(dir: &Path, list: &str, names: &[String]) -> Result<Catalog, String> {
    let listed = parse_list(list);
    if listed.groups.is_empty() {
        return Err("a lista de emoji veio vazia".to_owned());
    }
    let names: Vec<_> = names.iter().filter_map(|t| parse_names(t)).collect();
    let key = drawing_key(list);
    let known = std::fs::read(dir.join(DRAWN))
        .ok()
        .and_then(|b| serde_json::from_slice::<Drawn>(&b).ok())
        .filter(|d| d.key == key);
    if let Some(d) = known {
        let hidden: HashSet<String> = d.hidden.into_iter().collect();
        let own: HashMap<String, Face> = d
            .own
            .into_iter()
            .flat_map(|(face, list)| list.into_iter().map(move |t| (t, face)))
            .collect();
        let mut routes = Vec::new();
        let c = build(listed, &names, |t| {
            let route = match own.get(t) {
                _ if hidden.contains(t) => Route::Missing,
                Some(face) => Route::Own(*face),
                None => Route::Chain,
            };
            routes.push((t.to_owned(), route));
            route != Route::Missing
        });
        crate::emoji_font::shared().learn(&egui::FontFamily::Proportional, routes);
        return Ok(c);
    }
    let painter = Painter::window();
    let mut hidden = BTreeSet::new();
    let mut own: BTreeMap<Face, BTreeSet<String>> = BTreeMap::new();
    let c = build(listed, &names, |t| match painter.route(t) {
        Some(Route::Chain) => true,
        Some(Route::Own(face)) => {
            own.entry(face).or_default().insert(t.to_owned());
            true
        }
        _ => {
            hidden.insert(t.to_owned());
            false
        }
    });
    painter.fonts.rest();
    let drawn = Drawn {
        key,
        hidden: hidden.into_iter().collect(),
        own: own
            .into_iter()
            .map(|(face, list)| (face, list.into_iter().collect()))
            .collect(),
    };
    if let Ok(bytes) = serde_json::to_vec(&drawn)
        && let Err(e) = write_whole(&dir.join(DRAWN), &bytes)
    {
        crate::logln!("emoji: guardando {DRAWN}: {e}");
    }
    Ok(c)
}

/// The three files (at once), then the catalog, on a thread of its own.
async fn load(http: reqwest::Client, dir: PathBuf, server: &str) -> Result<Catalog, String> {
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut kept: Kept = std::fs::read(dir.join(KEPT))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let [list, names, derived] = sources(server);
    let old = |src: &Source| kept.files.get(src.file).cloned();
    let ((list_text, a), (names_text, b), (derived_text, c)) = tokio::join!(
        fetch(&http, &dir, old(&list), &list),
        fetch(&http, &dir, old(&names), &names),
        fetch(&http, &dir, old(&derived), &derived),
    );
    for (src, record) in [(&list, a), (&names, b), (&derived, c)] {
        if let Some(record) = record {
            kept.files.insert(src.file.to_owned(), record);
        }
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&kept)
        && let Err(e) = write_whole(&dir.join(KEPT), &bytes)
    {
        crate::logln!("emoji: guardando {KEPT}: {e}");
    }
    // Without the names the picker still works, in English.
    let found: Vec<String> = [names_text, derived_text]
        .into_iter()
        .filter_map(|t| t.map_err(|e| crate::logln!("emoji: {e}")).ok())
        .collect();
    let list = list_text.map_err(|e| format!("não deu para baixar a lista de emoji ({e})"))?;
    tokio::task::spawn_blocking(move || catalog(&dir, &list, &found))
        .await
        .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------- for the app

/// Where the catalog stands.
#[derive(Clone)]
pub enum Now {
    /// Not asked for yet, or on its way.
    Loading,
    Ready(Arc<Catalog>),
    Failed(String),
}

enum State {
    Idle,
    Loading,
    Ready(Arc<Catalog>),
    Failed(String),
}

/// The emoji catalog for the pickers, loaded once per run.
pub struct Emoji {
    dir: PathBuf,
    /// The chat server, which keeps copies of the files.
    server: String,
    http: reqwest::Client,
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    state: Mutex<State>,
}

impl Emoji {
    pub fn new(
        ctx: egui::Context,
        rt: tokio::runtime::Handle,
        http: reqwest::Client,
        dir: PathBuf,
        server: String,
    ) -> Arc<Emoji> {
        Arc::new(Emoji {
            dir,
            server,
            http,
            rt,
            ctx,
            state: Mutex::new(State::Idle),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts loading the catalog unless it is there or on its way; after a
    /// failure, tries again.
    pub fn want(self: &Arc<Self>) {
        {
            let mut st = self.lock();
            if matches!(*st, State::Loading | State::Ready(_)) {
                return;
            }
            *st = State::Loading;
        }
        let me = self.clone();
        self.rt.spawn(async move {
            let started = std::time::Instant::now();
            let result = load(me.http.clone(), me.dir.clone(), &me.server).await;
            *me.lock() = match result {
                Ok(c) => {
                    crate::logln!(
                        "emoji: {} da lista {} ({} que este PC não desenha) em {} ms",
                        c.entries.len(),
                        c.version,
                        c.hidden,
                        started.elapsed().as_millis()
                    );
                    State::Ready(Arc::new(c))
                }
                Err(e) => {
                    crate::logln!("emoji: {e}");
                    State::Failed(e)
                }
            };
            me.ctx.request_repaint();
        });
    }

    pub fn now(&self) -> Now {
        match &*self.lock() {
            State::Idle | State::Loading => Now::Loading,
            State::Ready(c) => Now::Ready(c.clone()),
            State::Failed(e) => Now::Failed(e.clone()),
        }
    }

    pub fn catalog(&self) -> Option<Arc<Catalog>> {
        match &*self.lock() {
            State::Ready(c) => Some(c.clone()),
            _ => None,
        }
    }

    /// For the test report.
    pub fn report(&self) -> Value {
        match self.now() {
            Now::Loading => json!({ "state": "loading" }),
            Now::Failed(e) => json!({ "state": "failed", "error": e }),
            Now::Ready(c) => json!({
                "state": "ready",
                "version": c.version,
                "shown": c.entries.len(),
                "withTones": c.entries.iter().filter(|e| e.tones.is_some()).count(),
                "withPairs": c.entries.iter().filter(|e| e.pairs.is_some()).count(),
                "hidden": c.hidden,
                "portuguese": c.portuguese,
                "groups": c.groups.iter().map(|g| json!({
                    "key": g.key, "label": g.label, "count": g.range.len(),
                })).collect::<Vec<_>>(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
# Version: 18.0
# group: Smileys & Emotion
# subgroup: face-smiling
1F600                                                  ; fully-qualified     # \u{1F600} E1.0 grinning face
2764 FE0F                                              ; fully-qualified     # \u{2764}\u{FE0F} E0.6 red heart
2764                                                   ; unqualified         # \u{2764} E0.6 red heart
# group: People & Body
1F44B                                                  ; fully-qualified     # \u{1F44B} E0.6 waving hand
1F44B 1F3FB                                            ; fully-qualified     # \u{1F44B}\u{1F3FB} E1.0 waving hand: light skin tone
1F44B 1F3FF                                            ; fully-qualified     # \u{1F44B}\u{1F3FF} E1.0 waving hand: dark skin tone
1F468 200D 1F9B0                                       ; fully-qualified     # \u{1F468}\u{200D}\u{1F9B0} E11.0 man: red hair
1F468 1F3FD 200D 1F9B0                                 ; fully-qualified     # \u{1F468}\u{1F3FD}\u{200D}\u{1F9B0} E11.0 man: medium skin tone, red hair
1F91D                                                  ; fully-qualified     # \u{1F91D} E3.0 handshake
1FAF1 1F3FB 200D 1FAF2 1F3FF                           ; fully-qualified     # \u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FF} E14.0 handshake: light skin tone, dark skin tone
# group: Component
1F3FB                                                  ; component           # \u{1F3FB} E1.0 light skin tone
# group: Flags
1F1E7 1F1F7                                            ; fully-qualified     # \u{1F1E7}\u{1F1F7} E2.0 flag: Brazil
";

    #[test]
    fn tones_go_to_the_emoji_they_are_tones_of() {
        assert_eq!(tone_of("grinning face"), Tone::None);
        assert_eq!(tone_of("flag: Brazil"), Tone::None);
        assert_eq!(
            tone_of("waving hand: medium-light skin tone"),
            Tone::One(1, "waving hand".into())
        );
        assert_eq!(
            tone_of("man: medium skin tone, red hair"),
            Tone::One(2, "man: red hair".into())
        );
        assert_eq!(
            tone_of("kiss: woman, man, dark skin tone"),
            Tone::One(4, "kiss: woman, man".into())
        );
        assert_eq!(
            tone_of("handshake: light skin tone, dark skin tone"),
            Tone::Two(0, 4, "handshake".into())
        );
        assert_eq!(
            tone_of("kiss: person, person, medium skin tone, light skin tone"),
            Tone::Two(2, 0, "kiss: person, person".into())
        );
    }

    /// A list with an emoji of two people in every combination of tones,
    /// the way emoji-test.txt has them: "handshake: light skin tone, dark
    /// skin tone", "kiss: person, person, light skin tone, dark skin tone".
    fn two_people_list(base: &str, base_name: &str, mixed_name: &str) -> String {
        let point = |t: usize| format!("{:X}", 0x1F3FB + t);
        let sep = if mixed_name.contains(": ") {
            ", "
        } else {
            ": "
        };
        let mut list = String::from("# Version: 18.0\n# group: People & Body\n");
        list += &format!("{base} ; fully-qualified # x E3.0 {base_name}\n");
        for (t, tone) in TONES.iter().enumerate() {
            list += &format!(
                "{base} {} ; fully-qualified # x E3.0 {base_name}: {tone}\n",
                point(t)
            );
        }
        for (a, first) in TONES.iter().enumerate() {
            for (b, second) in TONES.iter().enumerate().filter(|(b, _)| *b != a) {
                list += &format!(
                    "1FAF1 {} 200D 1FAF2 {} ; fully-qualified # x E14.0 {mixed_name}{sep}{first}, {second}\n",
                    point(a),
                    point(b),
                );
            }
        }
        list + "#EOF\n"
    }

    #[test]
    fn two_people_get_a_tone_each() {
        let list = two_people_list("1F91D", "handshake", "handshake");
        let c = build(parse_list(&list), &[], |_| true);
        let hand = c.entry_of("\u{1F91D}").unwrap();
        assert_eq!(hand.pair(3, 3), Some("\u{1F91D}\u{1F3FD}"));
        let light_dark = "\u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FF}";
        assert_eq!(hand.pair(1, 5), Some(light_dark));
        assert_eq!(hand.pair_of(light_dark), Some((1, 5)));
        assert_eq!(hand.pair_of("\u{1F91D}\u{1F3FD}"), None);
        assert_eq!(hand.pair(0, 5), None);
        // Found by any of them, and named with both tones.
        assert_eq!(
            c.entry_of(light_dark).map(|e| e.text.as_str()),
            Some("\u{1F91D}")
        );
        assert_eq!(
            c.name_of(light_dark).as_deref(),
            Some("handshake: pele clara e pele escura")
        );
        assert_eq!(c.hidden, 0);
        // One pair this PC cannot draw: no grid at all, and its 20 hidden.
        let c = build(parse_list(&list), &[], |t| t != light_dark);
        let hand = c.entry_of("\u{1F91D}").unwrap();
        assert!(hand.pairs.is_none() && hand.tones.is_some());
        assert_eq!(c.hidden, 20);
        // "kiss: person, person, ..." is the kiss.
        let list = two_people_list("1F48F", "kiss", "kiss: person, person");
        let c = build(parse_list(&list), &[], |_| true);
        assert!(c.entry_of("\u{1F48F}").unwrap().pairs.is_some());
    }

    #[test]
    fn the_list_is_read_group_by_group() {
        let l = parse_list(LIST);
        assert_eq!(l.version, "18.0");
        let names: Vec<&str> = l.groups.iter().map(|(g, _)| g.as_str()).collect();
        assert_eq!(names, ["Smileys & Emotion", "People & Body", "Flags"]);
        let smileys: Vec<&str> = l.groups[0].1.iter().map(|b| b.text.as_str()).collect();
        // The heart without its selector is not one keyboards offer.
        assert_eq!(smileys, ["\u{1F600}", "\u{2764}\u{FE0F}"]);
        let people = &l.groups[1].1;
        assert_eq!(people.len(), 3);
        assert_eq!(people[0].tones[0].as_deref(), Some("\u{1F44B}\u{1F3FB}"));
        assert_eq!(people[0].tones[4].as_deref(), Some("\u{1F44B}\u{1F3FF}"));
        assert_eq!(people[0].tones[2], None);
        assert_eq!(people[1].name, "man: red hair");
        assert_eq!(
            people[1].tones[2].as_deref(),
            Some("\u{1F468}\u{1F3FD}\u{200D}\u{1F9B0}")
        );
        // Two tones at once: a pair of the handshake.
        assert!(people[2].tones.iter().all(Option::is_none));
        assert_eq!(
            people[2].pairs[0][4].as_deref(),
            Some("\u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FF}")
        );
    }

    fn names() -> HashMap<String, Words> {
        let json = r#"{"annotations": {"identity": {"language": "pt"}, "annotations": {
            "😀": {"default": ["feliz", "rosto"], "tts": ["rosto risonho"]},
            "❤": {"default": ["amor", "coração"], "tts": ["coração vermelho"]},
            "👋": {"default": ["mão", "tchau"], "tts": ["mão acenando"]},
            "🏻": {"default": ["pele"], "tts": ["pele clarinha"]}
        }}}"#;
        parse_names(json).unwrap()
    }

    #[test]
    fn names_and_words_come_in_portuguese() {
        let derived = parse_names(
            r#"{"annotationsDerived": {"identity": {}, "annotations": {
                "🇧🇷": {"default": ["bandeira"], "tts": ["bandeira: Brasil"]}
            }}}"#,
        )
        .unwrap();
        let c = build(parse_list(LIST), &[names(), derived], |_| true);
        assert!(c.portuguese);
        assert_eq!(c.version, "18.0");
        let heart = c.entry_of("\u{2764}\u{FE0F}").unwrap();
        assert_eq!(heart.name, "coração vermelho");
        assert_eq!(
            c.entry_of("\u{1F1E7}\u{1F1F7}").unwrap().name,
            "bandeira: Brasil"
        );
        // Without a CLDR name, the list's.
        assert_eq!(c.entry_of("\u{1F91D}").unwrap().name, "handshake");
        // The tone names come from CLDR when it has them.
        assert_eq!(c.tone_names[0], "pele clarinha");
        assert_eq!(c.tone_names[4], "pele escura");
        let wave = c.entry_of("\u{1F44B}\u{1F3FF}").unwrap();
        assert_eq!(wave.text, "\u{1F44B}");
        assert_eq!(wave.with_tone(5), "\u{1F44B}\u{1F3FF}");
        assert_eq!(wave.with_tone(0), "\u{1F44B}");
        // A tone the list does not have is the emoji without one.
        assert_eq!(wave.with_tone(3), "\u{1F44B}");
        assert_eq!(
            c.name_of("\u{1F44B}\u{1F3FF}").as_deref(),
            Some("mão acenando: pele escura")
        );
        let labels: Vec<&str> = c.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Smileys e emoções", "Pessoas e corpo", "Bandeiras"]
        );
    }

    #[test]
    fn searching_folds_accents_and_names_come_first() {
        let c = build(parse_list(LIST), &[names()], |_| true);
        let found = |q: &str| -> Vec<String> {
            c.search(q)
                .into_iter()
                .map(|i| c.entries[i].text.clone())
                .collect()
        };
        assert_eq!(found("coracao"), ["\u{2764}\u{FE0F}"]);
        assert_eq!(found("CORAÇ"), ["\u{2764}\u{FE0F}"]);
        // In English too.
        assert_eq!(found("heart"), ["\u{2764}\u{FE0F}"]);
        assert_eq!(found("mão tch"), ["\u{1F44B}"]);
        // "rosto" names the grinning face and is a keyword of none other.
        assert_eq!(found("rosto"), ["\u{1F600}"]);
        assert!(found("xyz").is_empty());
        assert!(found("  ").is_empty());
        // Every word must start a word: "ração" is not "coração".
        assert!(found("racao").is_empty());
    }

    #[test]
    fn what_this_pc_does_not_draw_is_left_out() {
        // Pretend the dark tone and the flag cannot be drawn.
        let c = build(parse_list(LIST), &[], |t| {
            !t.contains('\u{1F3FF}') && !t.starts_with('\u{1F1E7}')
        });
        assert!(!c.portuguese);
        assert_eq!(c.entry_of("\u{1F1E7}\u{1F1F7}"), None);
        assert!(c.groups.iter().all(|g| g.key != "Flags"));
        let wave = c.entry_of("\u{1F44B}").unwrap();
        assert_eq!(wave.with_tone(5), "\u{1F44B}");
        assert_eq!(wave.with_tone(1), "\u{1F44B}\u{1F3FB}");
        // The flag, the one tone and the handshake of two tones, which is
        // not on offer without the other 19.
        assert_eq!(c.hidden, 3);
        assert_eq!(c.tone_names[0], "pele clara");
    }

    #[test]
    fn accents_fold() {
        assert_eq!(fold("Coração Ação Pão Índio"), "coracao acao pao indio");
        assert_eq!(fold("ÉÊÜÑ"), "eeun");
    }

    /// The fonts of this PC, and the ones the app carries: the heart, the
    /// thumb with its tone and a flag come out whole, and so do a family
    /// (one glyph per person), a handshake of two tones, the keycaps and the
    /// symbols with the emoji selector, Sark and Emoji 18.0 (emoji_font.rs).
    /// Letters do not, nor two emoji, nor a sequence no font knows (each
    /// piece is drawn on its own).
    #[test]
    fn the_painter_sees_what_the_fonts_draw() {
        if !Path::new(r"C:\Windows\Fonts\seguiemj.ttf").exists() {
            return;
        }
        let p = Painter::new();
        for t in [
            "\u{1F600}",
            "\u{2764}\u{FE0F}",
            "\u{1F44D}\u{1F3FD}",
            "\u{1F1E7}\u{1F1F7}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FF}",
            "#\u{FE0F}\u{20E3}",
            "\u{A9}\u{FE0F}",
            "\u{1F1E8}\u{1F1F6}",
            "\u{1FAEB}",
            "\u{1FAF9}\u{1F3FD}",
        ] {
            assert!(p.draws(t), "{t:?}");
        }
        for t in [
            "a",
            "\u{A9}",
            "\u{1F600}\u{1F600}",
            // A smiling face joined to a heart: no font has that sequence.
            "\u{1F600}\u{200D}\u{2764}\u{FE0F}",
        ] {
            assert!(!p.draws(t), "{t:?}");
        }
    }

    const FAKE_LIST: &str = "# emoji-test.txt\n# Version: 18.0\n# group: Flags\n\
        1F1E7 1F1F7 ; fully-qualified # \u{1F1E7}\u{1F1F7} E2.0 flag: Brazil\n#EOF\n";

    /// One emoji of each way of being drawn on this PC: by the app's fonts,
    /// by Segoe UI Emoji alone, by Noto, and by none (two faces joined).
    const WAYS: &str = "# Version: 18.0\n# group: Symbols\n\
        1F600 ; fully-qualified # x E1.0 grinning face\n\
        0023 FE0F 20E3 ; fully-qualified # x E0.6 keycap: #\n\
        1F600 200D 1F600 ; fully-qualified # x E99.0 two faces\n\
        # group: Flags\n\
        1F1E8 1F1F6 ; fully-qualified # x E16.0 flag: Sark\n#EOF\n";

    #[test]
    fn what_this_pc_draws_is_written_down() {
        if !Path::new(r"C:\Windows\Fonts\seguiemj.ttf").exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("babutela-emoji-d-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let texts =
            |c: &Catalog| -> Vec<String> { c.entries.iter().map(|e| e.text.clone()).collect() };
        let (keycap, sark) = ("#\u{FE0F}\u{20E3}", "\u{1F1E8}\u{1F1F6}");
        let c = catalog(&dir, WAYS, &[]).unwrap();
        assert_eq!(texts(&c), ["\u{1F600}", keycap, sark]);
        assert_eq!(c.hidden, 1);
        let d: Drawn = serde_json::from_slice(&std::fs::read(dir.join(DRAWN)).unwrap()).unwrap();
        assert_eq!(d.hidden, ["\u{1F600}\u{200D}\u{1F600}"]);
        assert_eq!(d.own[&Face::Segoe], [keycap]);
        assert_eq!(d.own[&Face::Noto], [sark]);
        assert_eq!(d.own.len(), 2);
        // The next run reads it instead of finding it out again.
        let changed = Drawn {
            hidden: vec!["\u{1F600}".into(), "\u{1F600}\u{200D}\u{1F600}".into()],
            ..d
        };
        std::fs::write(dir.join(DRAWN), serde_json::to_vec(&changed).unwrap()).unwrap();
        let c = catalog(&dir, WAYS, &[]).unwrap();
        assert_eq!(texts(&c), [keycap, sark]);
        // Another list: found out again.
        let other = WAYS.replace("E99.0 two faces", "E99.0 two grinning faces");
        let c = catalog(&dir, &other, &[]).unwrap();
        assert_eq!(texts(&c), ["\u{1F600}", keycap, sark]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Routes written down are taken as found, and the ones found here stay.
    #[test]
    fn routes_written_down_are_learned() {
        let f = EmojiFonts::new();
        let p = egui::FontFamily::Proportional;
        f.learn(
            &p,
            [
                ("\u{1F600}".to_owned(), Route::Missing),
                ("\u{1FADD}".to_owned(), Route::Own(Face::Noto)),
            ],
        );
        assert_eq!(f.route(&p, "\u{1F600}"), Route::Missing);
        assert_eq!(f.route(&p, "\u{1FADD}"), Route::Own(Face::Noto));
        f.learn(&p, [("\u{1F600}".to_owned(), Route::Chain)]);
        assert_eq!(f.route(&p, "\u{1F600}"), Route::Missing);
        // Text is never asked about.
        f.learn(&p, [("a".to_owned(), Route::Missing)]);
        assert_eq!(f.route(&p, "a"), Route::Text);
    }

    #[test]
    fn only_real_files_are_kept() {
        assert!(looks_right("emoji-test.txt", FAKE_LIST));
        assert!(!looks_right(
            "emoji-test.txt",
            "<html>Acesso bloqueado</html>"
        ));
        assert!(!looks_right(
            "emoji-test.txt",
            &FAKE_LIST.replace("#EOF", "")
        ));
        assert!(looks_right(
            "annotations.json",
            r#"{"annotations": {"annotations": {}}}"#
        ));
        assert!(!looks_right(
            "annotations.json",
            r#"{"annotations": {"annot"#
        ));
        assert!(!looks_right(
            "annotationsDerived.json",
            "404 page not found\n"
        ));
    }

    fn answer(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn the_servers_copy_comes_first_and_only_real_files_are_kept() {
        let dir = std::env::temp_dir().join(format!("babutela-emoji-m-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mirror = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let official = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let src = Source {
            file: "emoji-test.txt",
            urls: vec![
                format!(
                    "http://{}/emoji/emoji-test.txt",
                    mirror.local_addr().unwrap()
                ),
                format!("http://{}/emoji-test.txt", official.local_addr().unwrap()),
            ],
        };
        crate::net::install_crypto_provider();
        let http = reqwest::Client::new();
        // The server has no copy yet: the official one.
        let missing = answer("404 Not Found", "", "404 page not found\n");
        let list = answer("200 OK", "etag: \"u1\"\r\n", FAKE_LIST);
        let (got, _) = tokio::join!(fetch(&http, &dir, None, &src), async {
            serve_one(&mirror, &missing).await;
            serve_one(&official, &list).await
        });
        let (text, record) = (got.0.unwrap(), got.1.unwrap());
        assert_eq!(text, FAKE_LIST);
        assert_eq!(record.url, src.urls[1]);
        // A week later the server has it: from there now, asked without
        // the official address's ETag.
        let stale = KeptFile {
            checked: 0,
            ..record
        };
        let copy = answer("200 OK", "etag: \"m1\"\r\n", FAKE_LIST);
        let (got, head) = tokio::join!(
            fetch(&http, &dir, Some(stale), &src),
            serve_one(&mirror, &copy),
        );
        assert!(!head.contains("if-none-match"), "{head}");
        let record = got.1.unwrap();
        assert_eq!(record.url, src.urls[0]);
        assert_eq!(record.etag.as_deref(), Some("\"m1\""));
        // Another week: asked there whether it changed.
        let stale = KeptFile {
            checked: 0,
            ..record
        };
        let same = answer("304 Not Modified", "", "");
        let (got, head) = tokio::join!(
            fetch(&http, &dir, Some(stale.clone()), &src),
            serve_one(&mirror, &same),
        );
        assert!(head.contains("if-none-match: \"m1\""), "{head}");
        assert_eq!(got.0.unwrap(), FAKE_LIST);
        assert!(got.1.unwrap().checked > 0);
        // A page that is not the list (a proxy's), and the official
        // address gone: the copy kept, untouched.
        drop(official);
        let page = answer(
            "200 OK",
            "content-type: text/html\r\n",
            "<html>bloqueado</html>",
        );
        let (got, _) = tokio::join!(
            fetch(&http, &dir, Some(stale), &src),
            serve_one(&mirror, &page),
        );
        assert_eq!(got.0.unwrap(), FAKE_LIST);
        assert!(got.1.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.join(src.file)).unwrap(),
            FAKE_LIST
        );
        // Nothing kept, and nobody answering: an error.
        drop(mirror);
        let _ = std::fs::remove_file(dir.join(src.file));
        assert!(fetch(&http, &dir, None, &src).await.0.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Answers one request with `answer`, and gives back its head.
    async fn serve_one(listener: &tokio::net::TcpListener, answer: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut piece = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut piece).await.unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&piece[..n]);
        }
        sock.write_all(answer.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf).to_lowercase()
    }

    #[tokio::test]
    async fn a_kept_copy_is_used_and_asked_about_only_once_a_week() {
        let dir = std::env::temp_dir().join(format!("babutela-emoji-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let src = Source {
            file: "emoji-test.txt",
            urls: vec![format!(
                "http://{}/emoji-test.txt",
                listener.local_addr().unwrap()
            )],
        };
        crate::net::install_crypto_provider();
        let http = reqwest::Client::new();
        // The first time: downloaded, kept with its ETag.
        let first = answer("200 OK", "etag: \"v1\"\r\n", FAKE_LIST);
        let (got, head) =
            tokio::join!(fetch(&http, &dir, None, &src), serve_one(&listener, &first),);
        assert!(!head.contains("if-none-match"), "{head}");
        let (text, record) = (got.0.unwrap(), got.1.unwrap());
        assert_eq!(text, FAKE_LIST);
        assert_eq!(record.etag.as_deref(), Some("\"v1\""));
        assert_eq!(record.url, src.urls[0]);
        assert_eq!(
            std::fs::read_to_string(dir.join(src.file)).unwrap(),
            FAKE_LIST
        );
        // Fresh: no request at all (nobody is listening for one).
        let (again, none) = fetch(&http, &dir, Some(record.clone()), &src).await;
        assert_eq!(again.unwrap(), FAKE_LIST);
        assert!(none.is_none());
        // A week later: asked whether it changed, and it did not.
        let stale = KeptFile {
            checked: 0,
            ..record.clone()
        };
        let (got, head) = tokio::join!(
            fetch(&http, &dir, Some(stale.clone()), &src),
            serve_one(
                &listener,
                "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            ),
        );
        assert!(head.contains("if-none-match: \"v1\""), "{head}");
        assert_eq!(got.0.unwrap(), FAKE_LIST);
        assert!(got.1.unwrap().checked > 0);
        // The server is gone: the copy kept, however old.
        drop(listener);
        let (old, none) = fetch(&http, &dir, Some(stale), &src).await;
        assert_eq!(old.unwrap(), FAKE_LIST);
        assert!(none.is_none());
        // And with nothing kept, an error.
        let _ = std::fs::remove_file(dir.join(src.file));
        assert!(fetch(&http, &dir, None, &src).await.0.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real lists and this PC's fonts: `BABUTELA_EMOJI_DADOS` is a
    /// folder with the three files as the app downloads them.
    #[test]
    #[ignore]
    fn the_real_lists() {
        let Some(dir) = std::env::var_os("BABUTELA_EMOJI_DADOS") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).unwrap();
        let started = std::time::Instant::now();
        let listed = parse_list(&read("emoji-test.txt"));
        let names: Vec<_> = ["annotations.json", "annotationsDerived.json"]
            .iter()
            .filter_map(|n| parse_names(&read(n)))
            .collect();
        let parsed = started.elapsed();
        let painter = Painter::new();
        let mut checked = 0;
        let mut missing = Vec::new();
        let c = build(listed, &names, |t| {
            checked += 1;
            let drawn = painter.draws(t);
            if !drawn {
                missing.push(t.to_owned());
            }
            drawn
        });
        println!(
            "versão {} | {} emoji ({} com tons, {} com um tom para cada pessoa) | {} não desenhados | {} conferidos | leitura {} ms, tudo {} ms",
            c.version,
            c.entries.len(),
            c.entries.iter().filter(|e| e.tones.is_some()).count(),
            c.entries.iter().filter(|e| e.pairs.is_some()).count(),
            c.hidden,
            checked,
            parsed.as_millis(),
            started.elapsed().as_millis()
        );
        for g in &c.groups {
            println!("  {} ({}): {}", g.label, g.key, g.range.len());
        }
        missing.sort();
        missing.dedup();
        for t in &missing {
            let points: Vec<String> = t.chars().map(|ch| format!("{:04X}", ch as u32)).collect();
            println!("  não desenhado: {}", points.join(" "));
        }
        // How each one is drawn, per font (found above already).
        let mut by: HashMap<String, usize> = HashMap::new();
        for e in &c.entries {
            for t in std::iter::once(&e.text)
                .chain(e.tones.iter().flat_map(|t| t.iter()))
                .chain(e.pairs.iter().flat_map(|p| p.iter().flatten()))
            {
                let r = painter.route(t);
                *by.entry(format!("{r:?}")).or_default() += 1;
            }
        }
        println!("  como cada um é desenhado (com os tons): {by:?}");
        assert!(c.entries.len() > 1500);
        assert!(c.portuguese);
        let max = c
            .entries
            .iter()
            .flat_map(|e| {
                std::iter::once(&e.text)
                    .chain(e.tones.iter().flat_map(|t| t.iter()))
                    .chain(e.pairs.iter().flat_map(|p| p.iter().flatten()))
            })
            .map(|t| t.encode_utf16().count())
            .max()
            .unwrap();
        // The server takes reactions of up to 16 UTF-16 units.
        assert!(max <= 16, "{max}");
        let found: Vec<String> = c
            .search("coração")
            .into_iter()
            .take(5)
            .map(|i| c.entries[i].name.clone())
            .collect();
        println!("  “coração”: {found:?}");
    }
}
