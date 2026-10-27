//! The fonts on this PC by the names PowerPoint gives them, for drawing a
//! presentation in the faces it was made with: Windows' own, the ones the
//! person installed, and the ones Office downloads for itself. Aptos,
//! Office's font since 2023, is only in that last place: Windows does not
//! come with it.
//!
//! Only the start of each file is read (its list of tables, its names and
//! its style), so going through the 500 or so files of a Windows takes a
//! moment; a face is read whole only when a presentation uses it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Where a face is: its file, and which face of a collection (.ttc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Face {
    pub path: PathBuf,
    pub index: u32,
}

/// Every face found, by family (in lower case), bold and italic.
#[derive(Default)]
pub struct Fonts {
    faces: HashMap<(String, bool, bool), Face>,
    /// How many files were read, for the log.
    pub files: usize,
}

/// Files bigger than this are not fonts anyone presents with (the biggest
/// of Windows', for Chinese, are about 20 MB; the renderer takes at most
/// 32 MB a face).
const MAX_FILE: u64 = 40 * 1024 * 1024;

/// A collection holds at most this many faces here; a table of contents at
/// most this many tables. (Real ones hold a handful and about twenty.)
const MAX_FACES: u32 = 64;
const MAX_TABLES: usize = 256;

/// A face's names are read up to this size (they take a few KB).
const MAX_NAMES: u32 = 1024 * 1024;

/// The fonts as last found, and when. Going through them takes 40 ms once
/// Windows has the files at hand, but the first time after the PC starts
/// it took 3 s: the file picker starts it (`warm`), so it is done by the
/// time a file is chosen.
static FOUND: Mutex<Option<(Instant, Arc<Fonts>)>> = Mutex::new(None);

/// Fonts found this recently are used as they are.
const FRESH: Duration = Duration::from_secs(120);

/// What one face says about itself.
struct Named {
    face: Face,
    /// Its family as Windows' older font lists know it (name 1, which is
    /// what PowerPoint writes: "Calibri Light"), and as the family it
    /// belongs to (name 16: "Calibri"), when it says.
    family: Option<String>,
    typographic: Option<String>,
    bold: bool,
    italic: bool,
}

impl Fonts {
    /// The fonts of this PC, found again unless they were a moment ago
    /// (or are being found: this waits for that).
    pub fn current() -> Arc<Fonts> {
        let mut found = FOUND.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, fonts)) = found.as_ref()
            && at.elapsed() < FRESH
        {
            return fonts.clone();
        }
        let fonts = Arc::new(Fonts::scan());
        *found = Some((Instant::now(), fonts.clone()));
        fonts
    }

    /// Starts finding them, on a thread of its own: a presentation is on
    /// its way.
    pub fn warm() {
        let _ = std::thread::Builder::new()
            .name("slides-fontes".into())
            .spawn(|| {
                Fonts::current();
            });
    }

    /// The fonts in Windows' folder, the person's own and Office's.
    pub fn scan() -> Fonts {
        let mut files = Vec::new();
        let windows =
            std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        walk(&windows.join("Fonts"), &mut files, 0);
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            walk(&local.join(r"Microsoft\Windows\Fonts"), &mut files, 0);
            // Office's, downloaded as documents ask for them:
            // FontCache\<n>\CloudFonts\<family>\<number>.ttf.
            if let Ok(entries) = std::fs::read_dir(local.join(r"Microsoft\FontCache")) {
                for entry in entries.flatten() {
                    walk(&entry.path().join("CloudFonts"), &mut files, 0);
                }
            }
        }
        let named: Vec<Named> = files
            .iter()
            .filter_map(|path| read_names(path).ok())
            .flatten()
            .collect();
        let mut fonts = Fonts {
            faces: HashMap::new(),
            files: files.len(),
        };
        // The older family names first: "Calibri" is the regular Calibri,
        // not Calibri Light, whose family is also "Calibri" by name 16.
        for n in &named {
            if let Some(family) = &n.family {
                fonts
                    .faces
                    .entry((family.to_lowercase(), n.bold, n.italic))
                    .or_insert_with(|| n.face.clone());
            }
        }
        for n in &named {
            if let Some(family) = &n.typographic {
                fonts
                    .faces
                    .entry((family.to_lowercase(), n.bold, n.italic))
                    .or_insert_with(|| n.face.clone());
            }
        }
        fonts
    }

    /// The face of `family` in that style, if this PC has it.
    pub fn get(&self, family: &str, bold: bool, italic: bool) -> Option<&Face> {
        self.faces
            .get(&(family.trim().to_lowercase(), bold, italic))
    }

    pub fn len(&self) -> usize {
        self.faces.len()
    }
}

/// The family and style of a font already in memory (for embedded fonts).
pub struct FontInfo {
    pub family: String,
    pub bold: bool,
    pub italic: bool,
}

/// Reads the family name and style from an in-memory OpenType/TrueType
/// font (already deobfuscated). Returns `None` if the font is not valid.
pub fn font_info(data: &[u8]) -> Option<FontInfo> {
    if data.len() < 12 {
        return None;
    }
    let count = usize::from(be16(data.get(4..6)?)).min(MAX_TABLES);
    let records = data.get(12..12 + count * 16)?;
    let (bold, italic) = records
        .as_chunks::<16>()
        .0
        .iter()
        .find(|r| &r[..4] == b"OS/2")
        .and_then(|r| {
            let at = be32(&r[8..12]) as usize;
            data.get(at..at + 64)
        })
        .map(|os2| {
            let selection = be16(&os2[62..64]);
            (selection & 0x20 != 0, selection & 1 != 0)
        })
        .unwrap_or((false, false));
    let name_record = records
        .as_chunks::<16>()
        .0
        .iter()
        .find(|r| &r[..4] == b"name")?;
    let at = be32(&name_record[8..12]) as usize;
    let len = (be32(&name_record[12..16]) as usize).min(MAX_NAMES as usize);
    let table = data.get(at..at + len)?;
    // Name 1 (family) first, then 16 (typographic family).
    let family = name(table, 1).or_else(|| name(table, 16))?;
    Some(FontInfo {
        family,
        bold,
        italic,
    })
}

/// The bytes of a face, as a font file of its own: one face of a
/// collection is taken out of it.
pub fn load(face: &Face) -> Option<Vec<u8>> {
    let bytes = std::fs::read(&face.path).ok()?;
    if bytes.get(..4) != Some(b"ttcf") {
        return Some(bytes);
    }
    extract(&bytes, face.index)
}

/// Font files under `dir`, and a level of folders below it (Office keeps
/// a folder per family).
fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if depth < 2 {
                walk(&path, out, depth + 1);
            }
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(ext.as_deref(), Some("ttf" | "otf" | "ttc" | "otc")) {
            out.push(path);
        }
    }
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn read_at(file: &mut File, offset: u32, len: u32) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(u64::from(offset)))?;
    let mut buf = vec![0; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// A face's tables by tag: where each starts and how long it is.
fn tables(file: &mut File, offset: u32) -> std::io::Result<HashMap<[u8; 4], (u32, u32)>> {
    let head = read_at(file, offset, 12)?;
    let count = usize::from(be16(&head[4..6])).min(MAX_TABLES);
    let mut records = vec![0; count * 16];
    file.read_exact(&mut records)?;
    Ok(records
        .as_chunks::<16>()
        .0
        .iter()
        .map(|r| {
            (
                [r[0], r[1], r[2], r[3]],
                (be32(&r[8..12]), be32(&r[12..16])),
            )
        })
        .collect())
}

/// The names and style of every face in a font file.
fn read_names(path: &Path) -> std::io::Result<Vec<Named>> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() > MAX_FILE {
        return Ok(Vec::new());
    }
    let mut head = [0u8; 12];
    file.read_exact(&mut head)?;
    let starts: Vec<u32> = if &head[..4] == b"ttcf" {
        let n = be32(&head[8..12]).min(MAX_FACES);
        let mut offsets = vec![0; n as usize * 4];
        file.read_exact(&mut offsets)?;
        offsets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_be_bytes(*b))
            .collect()
    } else {
        vec![0]
    };
    let mut out = Vec::new();
    for (index, start) in starts.into_iter().enumerate() {
        let Ok(tables) = tables(&mut file, start) else {
            continue;
        };
        let Some(&(at, len)) = tables.get(b"name") else {
            continue;
        };
        let Ok(names) = read_at(&mut file, at, len.min(MAX_NAMES)) else {
            continue;
        };
        // Bold and italic as Windows files the face (OS/2 fsSelection's
        // bits 5 and 0), which is how PowerPoint asks for them.
        let (bold, italic) = match tables.get(b"OS/2") {
            Some(&(at, len)) if len >= 64 => match read_at(&mut file, at, 64) {
                Ok(os2) => {
                    let selection = be16(&os2[62..64]);
                    (selection & 0x20 != 0, selection & 1 != 0)
                }
                Err(_) => (false, false),
            },
            _ => (false, false),
        };
        out.push(Named {
            face: Face {
                path: path.to_owned(),
                index: index as u32,
            },
            family: name(&names, 1),
            typographic: name(&names, 16),
            bold,
            italic,
        });
    }
    Ok(out)
}

/// Name `id` of a face's name table, in American English if it has it,
/// else in its first language: Windows' own records (platform 3), whose
/// text is UTF-16. Symbol fonts (Wingdings, which PowerPoint's bullets
/// use) keep their names under encoding 0.
fn name(table: &[u8], id: u16) -> Option<String> {
    if table.len() < 6 {
        return None;
    }
    let count = usize::from(be16(&table[2..4]));
    let storage = usize::from(be16(&table[4..6]));
    let mut best: Option<(bool, String)> = None;
    for i in 0..count {
        let Some(r) = table.get(6 + i * 12..6 + i * 12 + 12) else {
            break;
        };
        let (platform, encoding, language) = (be16(&r[0..2]), be16(&r[2..4]), be16(&r[4..6]));
        if be16(&r[6..8]) != id || platform != 3 || !matches!(encoding, 0 | 1 | 10) {
            continue;
        }
        let english = language == 0x0409;
        if best.as_ref().is_some_and(|(e, _)| *e || !english) {
            continue;
        }
        let (len, at) = (usize::from(be16(&r[8..10])), usize::from(be16(&r[10..12])));
        let Some(bytes) = table.get(storage + at..storage + at + len) else {
            continue;
        };
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        let text = String::from_utf16_lossy(&units).trim().to_owned();
        if !text.is_empty() {
            best = Some((english, text));
        }
    }
    best.map(|(_, text)| text)
}

/// Face `index` of a collection as a font file of its own: its table of
/// contents, then its tables, each where the new contents says.
fn extract(ttc: &[u8], index: u32) -> Option<Vec<u8>> {
    let count = be32(ttc.get(8..12)?);
    if index >= count.min(MAX_FACES) {
        return None;
    }
    let at = 12 + index as usize * 4;
    let start = be32(ttc.get(at..at + 4)?) as usize;
    let head = ttc.get(start..start + 12)?;
    let tables = usize::from(be16(&head[4..6])).min(MAX_TABLES);
    let records = ttc.get(start + 12..start + 12 + tables * 16)?;
    let mut out = Vec::with_capacity(ttc.len().min(64 * 1024 * 1024));
    out.extend_from_slice(&head[..4]);
    out.extend_from_slice(&(tables as u16).to_be_bytes());
    out.extend_from_slice(&head[6..12]);
    let mut bodies = Vec::with_capacity(tables);
    let mut next = 12 + tables * 16;
    for r in records.as_chunks::<16>().0 {
        let (offset, len) = (be32(&r[8..12]) as usize, be32(&r[12..16]) as usize);
        let body = ttc.get(offset..offset.checked_add(len)?)?;
        out.extend_from_slice(&r[..8]);
        out.extend_from_slice(&u32::try_from(next).ok()?.to_be_bytes());
        out.extend_from_slice(&(len as u32).to_be_bytes());
        bodies.push(body);
        next += len.next_multiple_of(4);
    }
    for body in bodies {
        out.extend_from_slice(body);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    Some(out)
}
