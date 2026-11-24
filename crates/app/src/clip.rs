//! The Windows clipboard, for what egui does not do with it: Ctrl+V of a
//! picture, or of files copied in Explorer, into the composer, and
//! "Copiar imagem" on a picture in a conversation.
//!
//! egui-winit makes Ctrl+V a paste of text only when there is text: with a
//! picture and no text it hands egui an image no widget reads, and with
//! files copied in Explorer nothing at all, not even the key. So the app
//! takes the plain keys from egui-winit (`clipboard_shortcuts` off, main.rs)
//! and turns them into egui's clipboard events here, as egui-winit did
//! (`Keys`), deciding itself what a paste brings (ui/chat.rs `paste`).

use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use egui::{Event, Key, Modifiers};

/// What a clipboard key asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    Cut,
    Copy,
    /// `plain`: Ctrl+Shift+V, the text alone (as in browsers and Discord).
    Paste {
        plain: bool,
    },
}

/// The keys egui-winit takes for cut, copy and paste (its `is_*_command`,
/// in its order: Ctrl+Shift+Insert copies), and Ctrl+Shift+V.
pub fn shortcut(key: Key, m: Modifiers) -> Option<Shortcut> {
    if key == Key::Cut || (m.command_only() && key == Key::X) || (m.shift && key == Key::Delete) {
        Some(Shortcut::Cut)
    } else if key == Key::Copy
        || (m.command_only() && key == Key::C)
        || (m.ctrl && key == Key::Insert)
    {
        Some(Shortcut::Copy)
    } else if key == Key::Paste
        || (m.command_only() && key == Key::V)
        || (m.shift && key == Key::Insert)
    {
        Some(Shortcut::Paste { plain: false })
    } else if m.command && m.shift && !m.alt && key == Key::V {
        Some(Shortcut::Paste { plain: true })
    } else {
        None
    }
}

/// Turns a frame's clipboard keys into egui's clipboard events, in their
/// place among the others, as egui-winit does with its shortcuts on.
#[derive(Default)]
pub struct Keys {
    /// The keys held down. A paste repeated by holding them puts the text
    /// in again (as egui-winit did), but attaches nothing again.
    down: HashSet<Key>,
}

impl Keys {
    /// `paste(plain, again)` says what a paste brings: the event that takes
    /// its place, if any (`again`: the key is being held down).
    pub fn translate(
        &mut self,
        events: &mut Vec<Event>,
        mut paste: impl FnMut(bool, bool) -> Option<Event>,
    ) {
        let mut out = Vec::with_capacity(events.len());
        for event in std::mem::take(events) {
            match &event {
                Event::WindowFocused(false) => self.down.clear(),
                Event::Key {
                    key,
                    pressed: false,
                    ..
                } => {
                    self.down.remove(key);
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let again = !self.down.insert(*key);
                    if let Some(s) = shortcut(*key, *modifiers) {
                        match s {
                            Shortcut::Cut => out.push(Event::Cut),
                            Shortcut::Copy => out.push(Event::Copy),
                            Shortcut::Paste { plain } => out.extend(paste(plain, again)),
                        }
                        continue;
                    }
                }
                _ => {}
            }
            out.push(event);
        }
        *events = out;
    }
}

// ---------------------------------------------------------------- reading

/// The text on the clipboard, with Windows' line ends made plain (as
/// egui-winit pastes it), when there is any.
pub fn text() -> Option<String> {
    let text = arboard::Clipboard::new().ok()?.get_text().ok()?;
    let text = text.replace("\r\n", "\n");
    (!text.is_empty()).then_some(text)
}

/// The files copied in Explorer that are still there (folders left out).
pub fn files() -> Vec<PathBuf> {
    if !clipboard_win::raw::is_format_avail(clipboard_win::formats::CF_HDROP) {
        return Vec::new();
    }
    arboard::Clipboard::new()
        .and_then(|mut c| c.get().file_list())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}

/// Whether there is a picture: a PNG (browsers, the Snipping Tool) or a
/// bitmap (Windows makes each kind of bitmap from the others).
pub fn has_image() -> bool {
    use clipboard_win::{formats, raw};
    png_format().is_some_and(raw::is_format_avail)
        || [formats::CF_DIBV5, formats::CF_DIB, formats::CF_BITMAP]
            .into_iter()
            .any(raw::is_format_avail)
}

/// Text that is only an address, as a browser puts beside a picture it
/// copies (Firefox's "Copiar imagem" brings the picture's): with a picture,
/// the picture is what was meant. Any other text wins over a picture that
/// comes with it (Excel and OneNote add a picture of what was copied).
pub fn just_a_link(text: &str) -> bool {
    let t = text.trim();
    !t.contains(char::is_whitespace)
        && ["https://", "http://", "data:image/", "file:///"]
            .iter()
            .any(|p| {
                t.len() > p.len() && t.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p))
            })
}

/// The picture on the clipboard, written as `<dir>/<a folder of its
/// own>/<name>` (a PNG): as the program that copied it made it when it put
/// a PNG (transparency and all, nothing decoded), or else its bitmap, made
/// a PNG.
pub fn save_image(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let png = match raw_png() {
        Some(png) => png,
        None => {
            let img = arboard::Clipboard::new()
                .and_then(|mut c| c.get_image())
                .map_err(|e| e.to_string())?;
            let size = |n: usize| u32::try_from(n).map_err(|e| e.to_string());
            encode_png(size(img.width)?, size(img.height)?, img.bytes.into_owned())?
        }
    };
    let folder = dir.join(unique());
    std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let path = folder.join(name);
    std::fs::write(&path, &png).map_err(|e| e.to_string())?;
    Ok(path)
}

/// The "PNG" format programs register to put a PNG file on the clipboard.
fn png_format() -> Option<u32> {
    clipboard_win::raw::register_format("PNG").map(|f| f.get())
}

/// The PNG on the clipboard, as it was put there, when there is a sound one.
fn raw_png() -> Option<Vec<u8>> {
    let format = png_format()?;
    if !clipboard_win::raw::is_format_avail(format) {
        return None;
    }
    let _open = open()?;
    let mut data = Vec::new();
    clipboard_win::raw::get_vec(format, &mut data).ok()?;
    is_png(&data).then_some(data)
}

/// Opens the clipboard, trying again for a moment while another program
/// has it open (as browsers do: 5 ms apart).
fn open() -> Option<clipboard_win::Clipboard> {
    for _ in 0..10 {
        if let Ok(c) = clipboard_win::Clipboard::new() {
            return Some(c);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// A PNG of some size, by its header.
fn is_png(data: &[u8]) -> bool {
    data.starts_with(b"\x89PNG\r\n\x1a\n")
        && image::ImageReader::with_format(Cursor::new(data), image::ImageFormat::Png)
            .into_dimensions()
            .is_ok_and(|(w, h)| w > 0 && h > 0)
}

/// RGBA pixels as a PNG: without the alpha when nothing is transparent (a
/// quarter less to send), and made opaque when the alpha is zero all over
/// (some programs leave it unset in their bitmaps, and the picture would
/// come out invisible).
fn encode_png(w: u32, h: u32, mut rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    if w == 0 || h == 0 || rgba.len() != w as usize * h as usize * 4 {
        return Err("a imagem veio vazia".into());
    }
    if rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        for p in rgba.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
    }
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    let written = if rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255) {
        let rgb: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, _]| [r, g, b])
            .collect();
        encoder.write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
    } else {
        encoder.write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
    };
    written.map_err(|e| e.to_string())?;
    Ok(out)
}

/// A name no other paste of this run has, and none of an earlier run.
fn unique() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{ns:x}-{n}")
}

// ---------------------------------------------------------------- writing

/// Puts a picture (a file's bytes: PNG, JPEG, WebP, a GIF's first frame)
/// on the clipboard as a PNG and as a bitmap (arboard: the PNG first, which
/// browsers, Office and chat apps take, then a DIBV5, from which Windows
/// makes the older bitmaps Paint reads).
pub fn put_image(bytes: &[u8]) -> Result<(), String> {
    let (w, h) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_dimensions()
        .map_err(|e| e.to_string())?;
    if u64::from(w) * u64::from(h) > crate::images::MAX_PIXELS {
        return Err(format!("grande demais ({w}x{h})"));
    }
    let rgba = image::load_from_memory(bytes)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    let (w, h) = rgba.dimensions();
    arboard::Clipboard::new()
        .and_then(|mut c| {
            c.set_image(arboard::ImageData {
                width: w as usize,
                height: h as usize,
                bytes: rgba.into_raw().into(),
            })
        })
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- the folder

/// Where pasted pictures wait for their message: %LOCALAPPDATA%\Prosa\colados
/// (this PC's, as direto and updates are); a test profile (PROSA_USERDATA)
/// has its own.
pub fn dir(shell: &crate::shell::Shell) -> PathBuf {
    if shell.own_profile()
        && let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(local).join("Prosa").join("colados");
    }
    shell.path("colados")
}

/// The longest a pasted picture stays: one sent directly is served from
/// here for as long as others download it, and one left in a message
/// never sent waits for nothing.
pub const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 3600);

/// Removes the pasted pictures older than KEEP_FOR.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > KEEP_FOR);
        if old && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether `path` is a picture pasted here (in a folder of its own in `dir`).
pub fn is_pasted(dir: &Path, path: &Path) -> bool {
    path.parent().and_then(Path::parent) == Some(dir)
}

/// Deletes a pasted picture no longer needed, with its folder. Anything
/// else is left alone.
pub fn forget(dir: &Path, path: &Path) {
    if is_pasted(dir, path)
        && let Some(folder) = path.parent()
    {
        let _ = std::fs::remove_dir_all(folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: Modifiers = Modifiers {
        alt: false,
        ctrl: true,
        shift: false,
        mac_cmd: false,
        command: true,
    };

    fn key(key: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn the_clipboard_keys_are_egui_winits_and_ctrl_shift_v() {
        let shift = Modifiers::SHIFT;
        let ctrl_shift = Modifiers {
            shift: true,
            ..CTRL
        };
        let paste = Some(Shortcut::Paste { plain: false });
        assert_eq!(shortcut(Key::V, CTRL), paste);
        assert_eq!(shortcut(Key::Insert, shift), paste);
        assert_eq!(shortcut(Key::Paste, Modifiers::NONE), paste);
        assert_eq!(
            shortcut(Key::V, ctrl_shift),
            Some(Shortcut::Paste { plain: true })
        );
        assert_eq!(shortcut(Key::C, CTRL), Some(Shortcut::Copy));
        assert_eq!(shortcut(Key::Insert, CTRL), Some(Shortcut::Copy));
        // Ctrl+Shift+Insert copies: egui-winit asks about copying first.
        assert_eq!(shortcut(Key::Insert, ctrl_shift), Some(Shortcut::Copy));
        assert_eq!(shortcut(Key::X, CTRL), Some(Shortcut::Cut));
        assert_eq!(shortcut(Key::Delete, shift), Some(Shortcut::Cut));
        // AltGr is Ctrl+Alt on Windows: AltGr+V types, it does not paste.
        let alt_gr = Modifiers { alt: true, ..CTRL };
        assert_eq!(shortcut(Key::V, alt_gr), None);
        assert_eq!(shortcut(Key::V, Modifiers::NONE), None);
        assert_eq!(shortcut(Key::A, CTRL), None);
    }

    #[test]
    fn the_keys_become_clipboard_events_in_their_place() {
        let mut keys = Keys::default();
        let mut events = vec![
            Event::Text("a".into()),
            key(Key::C, true, CTRL),
            key(Key::C, false, CTRL),
            key(Key::V, true, CTRL),
            Event::Text("b".into()),
            key(Key::A, true, CTRL),
        ];
        let mut asked = Vec::new();
        keys.translate(&mut events, |plain, again| {
            asked.push((plain, again));
            Some(Event::Paste("colado".into()))
        });
        assert_eq!(asked, [(false, false)]);
        assert_eq!(
            events,
            vec![
                Event::Text("a".into()),
                Event::Copy,
                key(Key::C, false, CTRL),
                Event::Paste("colado".into()),
                Event::Text("b".into()),
                key(Key::A, true, CTRL),
            ]
        );
        // Held down: the next presses are "again" until it is let go, or
        // the window loses the keyboard (the release never comes then).
        let mut held = vec![key(Key::V, true, CTRL), key(Key::V, true, CTRL)];
        let mut asked = Vec::new();
        keys.translate(&mut held, |plain, again| {
            asked.push((plain, again));
            None
        });
        assert_eq!(asked, [(false, true), (false, true)]);
        assert!(held.is_empty(), "a paste of nothing leaves nothing");
        let mut back = vec![Event::WindowFocused(false), key(Key::V, true, CTRL)];
        let mut asked = Vec::new();
        keys.translate(&mut back, |plain, again| {
            asked.push((plain, again));
            None
        });
        assert_eq!(asked, [(false, false)]);
    }

    #[test]
    fn only_a_lone_address_counts_as_a_link() {
        assert!(just_a_link("https://exemplo.com/foto.png"));
        assert!(just_a_link("  HTTP://exemplo.com/a.jpg\n"));
        assert!(just_a_link("data:image/png;base64,iVBORw0KGgo="));
        assert!(!just_a_link("https://"));
        assert!(!just_a_link("veja https://exemplo.com"));
        assert!(!just_a_link("a\tb\nc\td"));
        assert!(!just_a_link("ação"));
        assert!(!just_a_link(""));
    }

    #[test]
    fn bitmaps_become_pngs_opaque_when_they_can() {
        let decode = |png: &[u8]| image::load_from_memory(png).unwrap();
        // Opaque: written without the alpha, pixels as they were.
        let opaque = [10, 20, 30, 255, 40, 50, 60, 255];
        let png = encode_png(2, 1, opaque.to_vec()).unwrap();
        let img = decode(&png);
        assert_eq!(img.color(), image::ColorType::Rgb8);
        assert_eq!(img.into_rgba8().into_raw(), opaque);
        // Some transparency: kept.
        let clear = [10, 20, 30, 255, 40, 50, 60, 0];
        let img = decode(&encode_png(2, 1, clear.to_vec()).unwrap());
        assert_eq!(img.into_rgba8().into_raw(), clear);
        // No alpha set at all: opaque, not invisible.
        let unset = [10, 20, 30, 0, 40, 50, 60, 0];
        let img = decode(&encode_png(2, 1, unset.to_vec()).unwrap());
        assert_eq!(img.into_rgba8().into_raw(), opaque);
        assert!(encode_png(0, 1, Vec::new()).is_err());
        assert!(encode_png(2, 2, vec![0; 4]).is_err());
        assert!(is_png(&png));
        assert!(!is_png(b"\x89PNG\r\n\x1a\nnada"));
    }

    #[test]
    fn pasted_pictures_go_when_done_and_nothing_else_does() {
        let root = std::env::temp_dir().join(format!("babutela-colados-{}", std::process::id()));
        let dir = root.join("colados");
        let _ = std::fs::remove_dir_all(&root);
        let folder = dir.join(unique());
        std::fs::create_dir_all(&folder).unwrap();
        let pasted = folder.join("imagem.png");
        std::fs::write(&pasted, b"x").unwrap();
        let mine = root.join("foto.png");
        std::fs::write(&mine, b"x").unwrap();
        assert!(is_pasted(&dir, &pasted));
        assert!(!is_pasted(&dir, &mine));
        assert!(!is_pasted(&dir, &dir.join("solto.png")));
        forget(&dir, &mine);
        assert!(mine.exists(), "a file of the person's own stays");
        // Recent: the sweep leaves it.
        sweep(&dir);
        assert!(pasted.exists());
        forget(&dir, &pasted);
        assert!(!folder.exists());
        assert!(dir.exists());
        assert_ne!(unique(), unique());
        let _ = std::fs::remove_dir_all(&root);
    }
}
