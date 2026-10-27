//! A .pptx drawn with BetterOffice (github.com/openooxml/betteroffice):
//! PowerPoint's file read, laid out and painted in Rust, with this PC's
//! fonts. Measured against PowerPoint's own export of 54 slides of its
//! bundled templates: text, tables, pictures and shapes come out nearly the
//! same (SSIM 0.96 on average); charts keep their numbers but not always
//! their colours, a few effects (reflections) are missing, and animations
//! are not drawn: each slide shows complete. At 1920 px a slide took 139 ms
//! on average (398 ms the slowest, full of photos), on one core.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Read};

use betteroffice_pptx::{Presentation, RenderOptions};

use super::fonts::{self, Fonts};
use super::timing;
use super::{MAX_H, MAX_W};

pub struct Pptx {
    deck: Presentation,
    /// The slides shown in a slide show (hidden ones are left out).
    shown: Vec<usize>,
    /// The raw .pptx bytes, kept to re-render slides with shapes hidden
    /// for build steps (click-by-click animations).
    bytes: Vec<u8>,
    scale: f32,
    size: [u32; 2],
    /// For each shown slide (index into `shown`), its build steps, when it
    /// has entrance animations.
    steps: HashMap<usize, timing::SlideSteps>,
}

impl Pptx {
    pub fn open(bytes: &[u8]) -> Result<Pptx, String> {
        let mut deck = Presentation::open(bytes).map_err(|e| {
            crate::logln!("apresentação: {e}");
            "Não deu para abrir essa apresentação.".to_owned()
        })?;
        let shown: Vec<usize> = (0..deck.slides().len())
            .filter(|&i| deck.slides()[i].hidden != Some(true))
            .collect();
        if shown.is_empty() {
            return Err("Essa apresentação não tem slides para mostrar.".into());
        }
        if shown.len() > super::wire::MAX_SLIDES {
            return Err(format!(
                "Essa apresentação tem {} slides: o máximo é {}.",
                shown.len(),
                super::wire::MAX_SLIDES
            ));
        }
        // Fonts embedded in the file (ppt/fonts/*.fntdata), deobfuscated
        // and registered before the system's. A presentation that carries
        // its fonts is drawn with them even on a PC that does not have
        // them.
        let embedded = register_embedded(&mut deck, bytes);
        let started = std::time::Instant::now();
        let fonts = Fonts::current();
        let mut done = HashSet::new();
        // The first face registered stands in for every family this PC
        // does not have, as Windows puts Arial in for them.
        register(&mut deck, &fonts, "Arial", &mut done);
        // Laid out once with that, the slides say which families they ask
        // for; those are registered and the layout starts over with them.
        let mut wanted = BTreeSet::new();
        let mut page = None;
        for &i in &shown {
            if let Ok(laid) = deck.render_slide(i) {
                page.get_or_insert((laid.display_list.width, laid.display_list.height));
                wanted.extend(
                    laid.font_substitutions
                        .into_iter()
                        .map(|s| s.requested_family),
                );
            }
        }
        let mut missing = Vec::new();
        for family in &wanted {
            if !register(&mut deck, &fonts, family, &mut done) {
                missing.push(family.as_str());
            }
        }
        crate::logln!(
            "apresentação: {} slides, fontes em {} ms ({} arquivos, {} faces, {} embutidas); {} pedidas, sem {:?}",
            shown.len(),
            started.elapsed().as_millis(),
            fonts.files,
            fonts.len(),
            embedded,
            wanted.len(),
            missing
        );
        let (w, h) = page.ok_or("Não deu para abrir essa apresentação.")?;
        if !(w > 0.0 && h > 0.0) {
            return Err("Não deu para abrir essa apresentação.".into());
        }
        let scale = (MAX_W as f32 / w).min(MAX_H as f32 / h);
        let size = [(w * scale).round() as u32, (h * scale).round() as u32];
        // Build steps: which slides have entrance animations, and which
        // shapes appear at each click.
        let deck_steps = timing::parse(bytes);
        let mut steps = HashMap::new();
        for (k, &i) in shown.iter().enumerate() {
            let part = &deck.slides()[i].part_path;
            if let Some(s) = deck_steps.get(part)
                && !s.clicks.is_empty()
            {
                steps.insert(k, s.clone());
            }
        }
        if !steps.is_empty() {
            crate::logln!(
                "apresentação: {} slides com passos de clique ({} passos no total)",
                steps.len(),
                steps.values().map(|s| s.clicks.len()).sum::<usize>()
            );
        }
        Ok(Pptx {
            deck,
            shown,
            bytes: bytes.to_vec(),
            scale,
            size,
            steps,
        })
    }

    pub fn count(&self) -> usize {
        self.shown.len()
    }

    /// How many images slide `k` produces: 1 for a normal slide, N+1 for
    /// a slide with N click steps.
    pub fn step_count(&self, k: usize) -> usize {
        self.steps.get(&k).map_or(1, timing::image_count)
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    pub fn notes(&self) -> Vec<String> {
        self.shown
            .iter()
            .map(|&i| self.deck.slides()[i].notes.trim().to_owned())
            .collect()
    }

    /// Slide `k` of the slide show as a PNG. For slides with build steps,
    /// this returns the final state (all shapes visible).
    pub fn draw(&self, k: usize) -> Result<Vec<u8>, String> {
        let options = RenderOptions {
            scale: self.scale,
            ..RenderOptions::default()
        };
        self.deck
            .render_png(self.shown[k], &options)
            .map(|png| png.bytes)
            .map_err(|e| e.to_string())
    }

    /// Slide `k` at build step `step` (0 = before any click). Returns the
    /// image with the shapes that haven't entered yet hidden. This opens a
    /// fresh Presentation with the shapes removed from the XML, so it is
    /// slower than `draw` (about 200 ms extra per step).
    pub fn draw_step(&self, k: usize, step: usize) -> Result<Vec<u8>, String> {
        let Some(steps) = self.steps.get(&k) else {
            return self.draw(k);
        };
        let Some(hidden) = timing::hidden_at_step(steps, step) else {
            return self.draw(k);
        };
        let slide_idx = self.shown[k];
        let part_path = &self.deck.slides()[slide_idx].part_path;
        let patched = hide_shapes_in_zip(&self.bytes, part_path, &hidden)?;
        let mut deck2 = Presentation::open(&patched).map_err(|e| {
            crate::logln!("apresentação: passo {step}: {e}");
            format!("passo {step}: {e}")
        })?;
        // Register the same fonts.
        let fonts = Fonts::current();
        let mut done = HashSet::new();
        register(&mut deck2, &fonts, "Arial", &mut done);
        for sub in self.deck.render_slide(slide_idx).iter() {
            for s in &sub.font_substitutions {
                register(&mut deck2, &fonts, &s.requested_family, &mut done);
            }
        }
        register_embedded(&mut deck2, &patched);
        let options = RenderOptions {
            scale: self.scale,
            ..RenderOptions::default()
        };
        deck2
            .render_png(slide_idx, &options)
            .map(|png| png.bytes)
            .map_err(|e| e.to_string())
    }
}

/// Gives the deck every style of `family` this PC has (once); false when
/// it has none.
fn register(
    deck: &mut Presentation,
    fonts: &Fonts,
    family: &str,
    done: &mut HashSet<String>,
) -> bool {
    let family = family.trim();
    if !done.insert(family.to_lowercase()) {
        return true;
    }
    let mut any = false;
    for (bold, italic) in [(false, false), (true, false), (false, true), (true, true)] {
        let Some(bytes) = fonts.get(family, bold, italic).and_then(fonts::load) else {
            continue;
        };
        match deck.register_font(family, bold, italic, &bytes) {
            Ok(_) => any = true,
            Err(e) => crate::logln!("apresentação: fonte {family}: {e}"),
        }
    }
    any
}

// ---------------------------------------------------------------- embedded fonts

/// Extracts and registers every .fntdata font embedded in the .pptx zip.
/// Returns how many faces were registered.
///
/// The .pptx embeds fonts in `ppt/fonts/` as `.fntdata` files. They are
/// OpenType/TrueType with the first 32 bytes XOR-obfuscated using the
/// GUID in the file name (the ODTTF scheme, per ECMA-376 Part 4 §2.8.1).
fn register_embedded(deck: &mut Presentation, pptx_bytes: &[u8]) -> usize {
    let Ok(archive) = zip::ZipArchive::new(Cursor::new(pptx_bytes)) else {
        return 0;
    };
    let mut count = 0;
    let names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    // Re-open: ZipArchive borrows mutably for by_name.
    let mut archive = match zip::ZipArchive::new(Cursor::new(pptx_bytes)) {
        Ok(a) => a,
        Err(_) => return 0,
    };
    for name in &names {
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with("ppt/fonts/") || !lower.ends_with(".fntdata") {
            continue;
        }
        let Ok(mut entry) = archive.by_name(name) else {
            continue;
        };
        let mut data = Vec::with_capacity(entry.size() as usize);
        if entry.read_to_end(&mut data).is_err() || data.len() < 32 {
            continue;
        }
        // The GUID is the file stem: "00A4B29F-2E5C-4B4A-B2D0-1A2B3C4D5E6F.fntdata"
        let stem = std::path::Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if !deobfuscate(&mut data, stem) {
            continue;
        }
        // Read family and style from the font itself.
        let Some(info) = fonts::font_info(&data) else {
            continue;
        };
        match deck.register_font(&info.family, info.bold, info.italic, &data) {
            Ok(_) => {
                crate::logln!(
                    "apresentação: fonte embutida \"{}\" ({}{}) registrada",
                    info.family,
                    if info.bold { "bold " } else { "" },
                    if info.italic { "italic" } else { "regular" }
                );
                count += 1;
            }
            Err(e) => crate::logln!("apresentação: fonte embutida \"{}\": {e}", info.family),
        }
    }
    count
}

/// Deobfuscates the first 32 bytes of `data` using the GUID `stem`.
/// Returns false when the GUID is not valid.
///
/// Algorithm (ECMA-376 / Wikipedia "ODTTF"): strip hyphens from the GUID,
/// take 32 hex chars → 16 bytes, reverse their order, XOR the first 32
/// bytes of the file with those 16 bytes repeated twice.
fn deobfuscate(data: &mut [u8], stem: &str) -> bool {
    let hex: String = stem.chars().filter(|c| *c != '-').collect();
    if hex.len() < 32 {
        return false;
    }
    let mut key = [0u8; 16];
    for (i, chunk) in hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .take(16)
        .enumerate()
    {
        let hi = hex_val(chunk[0]);
        let lo = hex_val(chunk[1]);
        if hi > 15 || lo > 15 {
            return false;
        }
        key[i] = hi << 4 | lo;
    }
    key.reverse();
    for (i, byte) in data.iter_mut().take(32).enumerate() {
        *byte ^= key[i % 16];
    }
    true
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0xFF,
    }
}

// ---------------------------------------------------------------- hiding shapes for build steps

/// Produces a copy of the .pptx with certain shapes removed from one
/// slide's XML. The shapes are identified by their `id` attribute in
/// `<p:cNvPr>`. Removing the whole `<p:sp>` or `<p:pic>` block is the
/// cleanest way: it does not affect the layout of the remaining shapes.
fn hide_shapes_in_zip(
    pptx_bytes: &[u8],
    slide_part: &str,
    hidden: &HashSet<&str>,
) -> Result<Vec<u8>, String> {
    let reader = Cursor::new(pptx_bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| format!("zip: {e}"))?;
    let mut out = Vec::with_capacity(pptx_bytes.len());
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut out));
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| format!("zip: {e}"))?;
            let name = entry.name().to_owned();
            let opts =
                zip::write::SimpleFileOptions::default().compression_method(entry.compression());
            writer
                .start_file(&name, opts)
                .map_err(|e| format!("zip: {e}"))?;
            if name == slide_part {
                let mut xml = String::new();
                entry
                    .read_to_string(&mut xml)
                    .map_err(|e| format!("xml: {e}"))?;
                let patched = remove_shapes(&xml, hidden);
                std::io::Write::write_all(&mut writer, patched.as_bytes())
                    .map_err(|e| format!("zip: {e}"))?;
            } else {
                std::io::copy(&mut entry, &mut writer).map_err(|e| format!("zip: {e}"))?;
            }
        }
        writer.finish().map_err(|e| format!("zip: {e}"))?;
    }
    Ok(out)
}

/// Removes `<p:sp>`, `<p:pic>`, and `<p:graphicFrame>` elements whose
/// `cNvPr id="N"` matches one of the hidden ids. A simple text scan: the
/// id is always in the first few hundred bytes of the element.
fn remove_shapes(xml: &str, hidden: &HashSet<&str>) -> String {
    let mut result = String::with_capacity(xml.len());
    let shape_tags = [
        "<p:sp>",
        "<p:sp ",
        "<p:pic>",
        "<p:pic ",
        "<p:graphicFrame>",
        "<p:graphicFrame ",
    ];
    let close_tags = ["</p:sp>", "</p:pic>", "</p:graphicFrame>"];

    let mut pos = 0;
    while pos < xml.len() {
        // Find the next shape-like opening tag.
        let mut found = None;
        for (tag_idx, tag) in shape_tags.iter().enumerate() {
            if xml[pos..].starts_with(tag) {
                found = Some(tag_idx / 2); // 0 = sp, 1 = pic, 2 = graphicFrame
                break;
            }
        }
        if let Some(kind) = found {
            let close = close_tags[kind];
            let end = xml[pos..].find(close).map(|i| pos + i + close.len());
            if let Some(end) = end {
                let element = &xml[pos..end];
                // Check if this element's cNvPr id matches a hidden shape.
                let should_hide = extract_cnvpr_id(element).is_some_and(|id| hidden.contains(id));
                if should_hide {
                    pos = end;
                    continue;
                }
            }
        }
        // Copy one byte (or one char) at a time. This is simple but
        // correct for well-formed XML.
        let ch = xml[pos..].chars().next().unwrap_or(' ');
        result.push(ch);
        pos += ch.len_utf8();
    }
    result
}

/// Extracts the `id` from `<p:cNvPr id="3" name="Title 1"/>` inside a
/// shape element.
fn extract_cnvpr_id(element: &str) -> Option<&str> {
    // Look for cNvPr (with any namespace prefix) and its id attribute.
    let cnv = element.find("cNvPr ")?;
    let rest = &element[cnv..];
    let id_attr = rest.find(" id=\"")?;
    let start = id_attr + 5; // length of ` id="`
    let end = rest[start..].find('"')? + start;
    Some(&rest[start..end])
}
