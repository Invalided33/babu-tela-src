//! Emoji drawn whole wherever they show: messages, names, reactions and the
//! pickers.
//!
//! egui (epaint 0.37) picks the font of each grapheme cluster by its first
//! character: the first face of the app's chain that has it (theme.rs:
//! Segoe UI, the flags, Segoe UI Emoji, Segoe UI Symbol). Three kinds of
//! emoji came out wrong that way:
//! - Keycaps and symbols with the emoji selector. #️⃣ is "#", U+FE0F and
//!   U+20E3, and ©️ ™️ ♥️ ‼️ ↔️ ▪️ start with a character Segoe UI has, so
//!   they came out in black and white, in the text's face. UTS #51 says the
//!   selector asks for the emoji, and the browsers look for a colour font
//!   for it (Chromium's emoji segmentation, Firefox's gfxFontGroup); egui
//!   does not yet (github.com/emilk/egui/issues/8559).
//! - Emoji Windows' emoji font does not have: Emoji 18.0 on Windows 11 in
//!   October 2026, and on Windows 10, whose font stopped at Emoji 12, some
//!   600 more. They came out as boxes, or as Segoe UI Symbol's outlines.
//! - Flags the flag font does not have (Sark, of Emoji 16.0): two letters.
//!
//! [`rasterizer`] is asked about every cluster before any font (epaint's
//! `FontPriority::Highest`). Text, and the emoji the chain already draws
//! whole, go on the usual way, untouched. The others are laid out in one
//! emoji font alone, Segoe UI Emoji first, then the flags, then Noto Color
//! Emoji (Google's, which the app carries: the only font with all of
//! Emoji 18.0), and egui gets the picture, placed where it would have put a
//! glyph of that font in the line.
//!
//! Which way each emoji goes is found once, by laying it out at 8 px in
//! each font: under a millisecond each, but a picker full of emoji never
//! seen would wait for all of them in one frame. The emoji list's check
//! (emoji.rs) finds them for the whole list away from the window's thread,
//! through the same fonts ([`shared`]), and writes them down for the next
//! runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use egui::{Color32, ColorImage, FontFamily, FontId, vec2};
use epaint::text::{
    FontData, FontDefinitions, FontPriority, Fonts, Galley, Glyph, GlyphBitmap, GlyphRasterizer,
    GlyphRasterizerRequest, LayoutJob, RasterizedGlyph, TextOptions, has_emoji_presentation,
};
use serde::{Deserialize, Serialize};

/// Noto Color Emoji 2.057 (COLRv1), the Unicode 18.0 release of
/// github.com/googlefonts/noto-emoji (tag v2026-09-24-unicode18_0), © 2022
/// Google Inc., under the SIL Open Font License 1.1
/// (assets/fonts/NotoColorEmoji-OFL.txt, shown under Configurações →
/// Licenças). build.rs unpacks it.
const NOTO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/noto.ttf"));

/// Its licence, which goes with every copy of the font (the app shows it).
pub const NOTO_LICENSE: &str = include_str!("../assets/fonts/NotoColorEmoji-OFL.txt");

/// How emoji are drawn, for the answers written down about what this PC
/// draws (emoji.rs): a change here has to find them out again, even in a
/// build with the same version number.
pub const METHOD: &str =
    "3: Segoe UI Emoji, bandeiras, Noto Color Emoji 2.057; inteiros; rotas guardadas";

/// The size routes are decided at: it does not change which glyphs there
/// are, and small ones cost the least to draw into the atlas.
const PROBE: f32 = 8.0;

/// How tall the atlas of the fonts here may grow (it is 2048 wide) before
/// they start over: 4 MB. Only the pictures egui keeps are needed.
const ATLAS_LIMIT: usize = 512;

/// The fonts an emoji can be drawn from on its own, in the order tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Face {
    Segoe,
    Flags,
    Noto,
}

impl Face {
    const ALL: [Face; 3] = [Face::Segoe, Face::Flags, Face::Noto];

    /// Its name among the fonts (theme.rs names the first two).
    fn font(self) -> &'static str {
        match self {
            Face::Segoe => "emoji",
            Face::Flags => "flags",
            Face::Noto => "noto",
        }
    }

    /// A family of that font alone.
    fn family(self) -> FontFamily {
        FontFamily::Name(
            match self {
                Face::Segoe => "so-segoe-emoji",
                Face::Flags => "so-bandeiras",
                Face::Noto => "so-noto",
            }
            .into(),
        )
    }
}

/// How one grapheme cluster is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Not an emoji: the fonts' business.
    Text,
    /// An emoji the app's chain already draws whole.
    Chain,
    /// An emoji drawn here, from this font alone.
    Own(Face),
    /// An emoji nothing draws whole: the chain does what it can.
    Missing,
}

/// Whether a cluster is an emoji, or asks to be one: what has emoji
/// presentation by Unicode's tables (UTS #51: the emoji selector, keycaps,
/// flags, skin tones, the emoji shown as emoji by default), and anything
/// starting in the blocks new emoji go to, which epaint's tables (Unicode
/// 17) do not know yet. The text selector (U+FE0E) asks for text.
pub fn emoji_like(cluster: &str) -> bool {
    !cluster.is_ascii()
        && !cluster.contains('\u{FE0E}')
        && (has_emoji_presentation(cluster)
            || cluster
                .chars()
                .next()
                .is_some_and(|c| ('\u{1F000}'..='\u{1FAFF}').contains(&c)))
}

/// Lays emoji out in the app's fonts and in each emoji font alone, and
/// draws the ones only an emoji font alone draws whole.
pub struct EmojiFonts(Mutex<Inner>);

struct Inner {
    defs: FontDefinitions,
    fonts: Fonts,
    /// The fonts of [`Face::ALL`] there are (Segoe UI Emoji may be missing).
    faces: Vec<Face>,
    /// The app's families; a cluster asked about in another is decided as
    /// if in the proportional one.
    families: Vec<FontFamily>,
    routes: HashMap<FontFamily, HashMap<String, Route>>,
    /// A plain emoji's advance at [`PROBE`], per family.
    em: HashMap<FontFamily, f32>,
    /// The ascent and the line height of a family's first face, per family
    /// and size in pixels.
    hosts: HashMap<(FontFamily, u32), (f32, f32)>,
}

impl EmojiFonts {
    pub fn new() -> EmojiFonts {
        let mut defs = crate::theme::font_definitions();
        let families = defs.families.keys().cloned().collect();
        defs.font_data.insert(
            Face::Noto.font().to_owned(),
            Arc::new(FontData::from_static(NOTO)),
        );
        let faces: Vec<Face> = Face::ALL
            .into_iter()
            .filter(|f| defs.font_data.contains_key(f.font()))
            .collect();
        for f in &faces {
            defs.families.insert(f.family(), vec![f.font().to_owned()]);
        }
        EmojiFonts(Mutex::new(Inner {
            fonts: Fonts::new(TextOptions::default(), defs.clone()),
            defs,
            faces,
            families,
            routes: HashMap::new(),
            em: HashMap::new(),
            hosts: HashMap::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How a cluster shows in `family`.
    pub fn route(&self, family: &FontFamily, cluster: &str) -> Route {
        if !emoji_like(cluster) {
            return Route::Text;
        }
        self.lock().route(family, cluster)
    }

    /// Takes answers found before (written down by emoji.rs), keeping any
    /// found here already.
    pub fn learn(&self, family: &FontFamily, routes: impl IntoIterator<Item = (String, Route)>) {
        let mut inner = self.lock();
        let family = inner.family_of(family);
        let known = inner.routes.entry(family).or_default();
        for (cluster, route) in routes {
            known.entry(cluster).or_insert(route);
        }
    }

    /// Lets go of the glyphs laid out to find routes (megabytes, after the
    /// whole list); the routes stay.
    pub fn rest(&self) {
        let mut inner = self.lock();
        let defs = inner.defs.clone();
        inner.fonts = Fonts::new(TextOptions::default(), defs);
    }

    /// egui's question: a picture of this cluster, or None for the fonts.
    fn rasterize(&self, req: &GlyphRasterizerRequest<'_>) -> Option<RasterizedGlyph> {
        if !emoji_like(req.cluster) {
            return None;
        }
        let mut inner = self.lock();
        match inner.route(req.family, req.cluster) {
            Route::Own(face) => inner.draw(face, req.family, req.cluster, req.font_size_px),
            Route::Text | Route::Chain | Route::Missing => None,
        }
    }
}

/// The window's emoji fonts. The emoji list's check (emoji.rs) runs through
/// them too, away from the window's thread, so the answers it finds are the
/// window's before the picker opens: found in a frame, the 90 emoji of the
/// picker's first screen took it from 38 to 69 ms.
pub fn shared() -> Arc<EmojiFonts> {
    static SHARED: OnceLock<Arc<EmojiFonts>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(EmojiFonts::new())).clone()
}

/// egui's rasterizer (theme::apply), asked before any font.
pub fn rasterizer() -> GlyphRasterizer {
    rasterizer_of(shared())
}

fn rasterizer_of(fonts: Arc<EmojiFonts>) -> GlyphRasterizer {
    GlyphRasterizer::new(
        "babutela::emoji",
        move |req: &GlyphRasterizerRequest<'_>| fonts.rasterize(req),
    )
    .with_priority(FontPriority::Highest)
}

impl Inner {
    fn family_of(&self, family: &FontFamily) -> FontFamily {
        if self.families.contains(family) {
            family.clone()
        } else {
            FontFamily::Proportional
        }
    }

    /// Starts the fonts over once their atlas has grown past
    /// [`ATLAS_LIMIT`]: what is laid out here is only looked at once, and
    /// egui keeps its own copy of each picture. Before laying anything out:
    /// an atlas at its limit writes new glyphs over old ones, which a
    /// picture could then read.
    fn trim(&mut self) {
        if self.fonts.font_image_size()[1] > ATLAS_LIMIT {
            self.fonts = Fonts::new(TextOptions::default(), self.defs.clone());
        }
    }

    fn route(&mut self, family: &FontFamily, cluster: &str) -> Route {
        let family = self.family_of(family);
        if let Some(r) = self.routes.get(&family).and_then(|m| m.get(cluster)) {
            return *r;
        }
        self.trim();
        let r = if self.whole_in(&family, cluster) {
            Route::Chain
        } else {
            self.faces
                .clone()
                .into_iter()
                .find(|f| self.whole_in(&f.family(), cluster))
                .map_or(Route::Missing, Route::Own)
        };
        self.routes
            .entry(family)
            .or_default()
            .insert(cluster.to_owned(), r);
        r
    }

    fn whole_in(&mut self, family: &FontFamily, cluster: &str) -> bool {
        let em = self.em(family);
        let g = lay(&mut self.fonts, cluster, family, PROBE);
        whole(cluster, &g, em)
    }

    fn em(&mut self, family: &FontFamily) -> f32 {
        if let Some(e) = self.em.get(family) {
            return *e;
        }
        // The flag font has nothing but flags.
        let plain = if *family == Face::Flags.family() {
            "\u{1F1E7}\u{1F1F7}"
        } else {
            "\u{1F600}"
        };
        let e = advance(&lay(&mut self.fonts, plain, family, PROBE)).max(PROBE / 2.0);
        self.em.insert(family.clone(), e);
        e
    }

    /// Where egui puts a glyph of another face in a line of `family` at
    /// `size` pixels: its first face's ascent and line height.
    fn host(&mut self, family: &FontFamily, size: f32) -> (f32, f32) {
        let key = (family.clone(), size.to_bits());
        if let Some(m) = self.hosts.get(&key) {
            return *m;
        }
        let g = lay(&mut self.fonts, "a", family, size);
        let m = g
            .rows
            .first()
            .and_then(|r| r.glyphs.first())
            .map_or((size, size * 1.33), |g| (g.font_ascent, g.font_height));
        self.hosts.insert(key, m);
        m
    }

    fn draw(
        &mut self,
        face: Face,
        host: &FontFamily,
        cluster: &str,
        size: f32,
    ) -> Option<RasterizedGlyph> {
        if !(size > 0.0 && size.is_finite()) {
            return None;
        }
        self.trim();
        let host = self.family_of(host);
        let (ascent, height) = self.host(&host, size);
        let g = lay(&mut self.fonts, cluster, &face.family(), size);
        picture(&g, self.fonts.texture_atlas().image(), ascent, height)
    }
}

/// One line of text, laid out at `size` pixels.
fn lay(fonts: &mut Fonts, text: &str, family: &FontFamily, size: f32) -> Galley {
    let job = LayoutJob::simple_singleline(
        text.to_owned(),
        FontId::new(size, family.clone()),
        Color32::WHITE,
    );
    fonts.layout_uncached(1.0, Arc::new(job))
}

fn advance(g: &Galley) -> f32 {
    g.rows
        .iter()
        .flat_map(|r| r.glyphs.iter())
        .map(|g| g.advance_width)
        .sum()
}

fn inked(g: &Galley) -> Vec<&Glyph> {
    g.rows
        .iter()
        .flat_map(|r| r.glyphs.iter())
        .filter(|g| !g.uv_rect.is_nothing())
        .collect()
}

/// Two regional indicators: a flag, which a font that has it draws as one
/// glyph (the flag font draws the letters of one it lacks in colour too).
fn is_flag(cluster: &str) -> bool {
    cluster
        .chars()
        .all(|c| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c))
}

/// Whether a cluster laid out came out as one colour picture: one glyph,
/// or colour glyphs drawn over one another within an emoji's width (Segoe
/// UI Emoji puts a family together from one glyph per person, and a couple
/// of two skin tones from one glyph per person too). `em` is a plain
/// emoji's advance in the same font. Pieces side by side take more room
/// than one emoji; a box or a black and white outline is not in colour.
fn whole(cluster: &str, g: &Galley, em: f32) -> bool {
    let inked = inked(g);
    let advance = advance(g);
    g.rows.len() == 1
        && !inked.is_empty()
        && inked.iter().all(|g| g.is_color)
        && advance > 0.0
        && advance <= 1.2 * em
        && (inked.len() == 1 || !is_flag(cluster))
}

/// The glyphs of one cluster laid out in one font alone, as one picture for
/// egui. A picture's origin is the pen on the baseline of the line it goes
/// in, whose first face has `ascent` and line height `height` (`host`);
/// this one sits where egui puts a glyph of another face in such a line,
/// centred on their difference of height (epaint's text_layout).
fn picture(g: &Galley, atlas: &ColorImage, ascent: f32, height: f32) -> Option<RasterizedGlyph> {
    let [row] = &g.rows[..] else {
        return None;
    };
    let inked = inked(g);
    let first = inked.first()?;
    if !inked.iter().all(|g| g.is_color) {
        return None;
    }
    let rects: Vec<(i64, i64, usize, usize, &Glyph)> = inked
        .iter()
        .map(|gl| {
            let uv = gl.uv_rect;
            (
                (row.pos.x + gl.pos.x + uv.offset.x).round() as i64,
                (row.pos.y + gl.pos.y + uv.offset.y).round() as i64,
                usize::from(uv.max[0] - uv.min[0]),
                usize::from(uv.max[1] - uv.min[1]),
                *gl,
            )
        })
        .collect();
    let x0 = rects.iter().map(|r| r.0).min()?;
    let y0 = rects.iter().map(|r| r.1).min()?;
    let x1 = rects.iter().map(|r| r.0 + r.2 as i64).max()?;
    let y1 = rects.iter().map(|r| r.1 + r.3 as i64).max()?;
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let mut pixels = vec![Color32::TRANSPARENT; w * h];
    let side = atlas.size[0];
    for (x, y, rw, rh, gl) in &rects {
        let uv = gl.uv_rect;
        let (left, top) = ((x - x0) as usize, (y - y0) as usize);
        for j in 0..*rh {
            for i in 0..*rw {
                let from = (usize::from(uv.min[1]) + j) * side + usize::from(uv.min[0]) + i;
                let (Some(src), at) = (atlas.pixels.get(from), (top + j) * w + left + i) else {
                    continue;
                };
                pixels[at] = over(*src, pixels[at]);
            }
        }
    }
    // The font's baseline in the laid out line, and where it goes in the
    // host's line: its ascent below the top, moved by half the difference
    // between the two line heights.
    let baseline = row.pos.y + first.pos.y;
    let shift = first.font_face_ascent + 0.5 * (height - first.font_face_height) - ascent;
    Some(RasterizedGlyph {
        bitmap: GlyphBitmap {
            image: ColorImage::new([w, h], pixels),
            offset_px: vec2(x0 as f32 - row.pos.x, y0 as f32 - baseline + shift),
            is_color: true,
        },
        advance_px: advance(g),
    })
}

/// `src` over `dst`, both premultiplied.
fn over(src: Color32, dst: Color32) -> Color32 {
    let keep = 255 - u32::from(src.a());
    let mix = |s: u8, d: u8| (u32::from(s) + (u32::from(d) * keep + 127) / 255).min(255) as u8;
    Color32::from_rgba_premultiplied(
        mix(src.r(), dst.r()),
        mix(src.g(), dst.g()),
        mix(src.b(), dst.b()),
        mix(src.a(), dst.a()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_fonts() -> bool {
        std::path::Path::new(r"C:\Windows\Fonts\seguiemj.ttf").exists()
    }

    #[test]
    fn text_is_left_alone() {
        for t in [
            "a",
            "#",
            "1",
            "\u{A9}",
            "\u{2122}",
            "\u{2192}",
            "\u{263A}\u{FE0E}",
            "Olá",
        ] {
            assert!(!emoji_like(t), "{t:?}");
        }
        for t in [
            "\u{1F600}",
            "#\u{FE0F}\u{20E3}",
            "\u{A9}\u{FE0F}",
            "\u{1F1E8}\u{1F1F6}",
            // Unicode 18.0, which epaint's tables do not know.
            "\u{1FAEB}",
        ] {
            assert!(emoji_like(t), "{t:?}");
        }
    }

    /// What goes where with this PC's fonts: Windows 11 in October 2026,
    /// whose Segoe UI Emoji has Emoji 17.0. On a Windows that draws Emoji
    /// 18.0 the cracking face goes the chain's way, which is right too.
    #[test]
    fn emoji_go_to_the_font_that_draws_them_whole() {
        if !windows_fonts() {
            return;
        }
        let f = EmojiFonts::new();
        let p = FontFamily::Proportional;
        let route = |t: &str| f.route(&p, t);
        assert_eq!(route("a"), Route::Text);
        assert_eq!(route("\u{A9}"), Route::Text);
        assert_eq!(route("\u{1F600}"), Route::Chain);
        assert_eq!(route("\u{2764}\u{FE0F}"), Route::Chain);
        assert_eq!(route("\u{1F44D}\u{1F3FD}"), Route::Chain);
        assert_eq!(route("\u{1F1E7}\u{1F1F7}"), Route::Chain);
        // A family, one glyph per person, over one another.
        assert_eq!(
            route("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
            Route::Chain
        );
        // A handshake of two skin tones.
        assert_eq!(
            route("\u{1FAF1}\u{1F3FB}\u{200D}\u{1FAF2}\u{1F3FF}"),
            Route::Chain
        );
        // Segoe UI has "#" and "©": the emoji come from Segoe UI Emoji.
        for t in [
            "#\u{FE0F}\u{20E3}",
            "\u{A9}\u{FE0F}",
            "\u{2665}\u{FE0F}",
            "\u{2122}\u{FE0F}",
        ] {
            assert_eq!(route(t), Route::Own(Face::Segoe), "{t:?}");
            assert_eq!(f.route(&FontFamily::Monospace, t), Route::Own(Face::Segoe));
            assert_eq!(
                f.route(&crate::theme::bold_family(), t),
                Route::Own(Face::Segoe)
            );
        }
        // Sark: no flag in the flag font, nor in Segoe UI Emoji.
        assert_eq!(route("\u{1F1E8}\u{1F1F6}"), Route::Own(Face::Noto));
        assert!(matches!(
            route("\u{1FAEB}"),
            Route::Chain | Route::Own(Face::Noto)
        ));
        // Two emoji joined in a way nobody draws as one.
        assert_eq!(route("\u{1F600}\u{200D}\u{1F600}"), Route::Missing);
    }

    /// The pictures sit where the chain's emoji sit: a keycap (a picture)
    /// next to a smiley (a glyph of the chain), in each of the app's
    /// families, at a few sizes.
    #[test]
    fn pictures_line_up_with_the_chains_emoji() {
        if !windows_fonts() {
            return;
        }
        let mut fonts = Fonts::new(TextOptions::default(), crate::theme::font_definitions())
            .with_glyph_rasterizer(rasterizer_of(Arc::new(EmojiFonts::new())));
        for family in [
            FontFamily::Proportional,
            FontFamily::Monospace,
            crate::theme::bold_family(),
        ] {
            // Windows' display scales too: the pictures come in pixels.
            for (size, scale) in [
                (14.0, 1.0),
                (22.0, 1.0),
                (33.0, 1.0),
                (14.0, 1.25),
                (22.0, 1.5),
                (14.0, 2.0),
            ] {
                let job = LayoutJob::simple_singleline(
                    "x\u{1F600}#\u{FE0F}\u{20E3}".to_owned(),
                    FontId::new(size, family.clone()),
                    Color32::WHITE,
                );
                let g = fonts.layout_uncached(scale, Arc::new(job));
                let row = &g.rows[0];
                let ink: Vec<&Glyph> = row
                    .glyphs
                    .iter()
                    .filter(|g| !g.uv_rect.is_nothing())
                    .collect();
                assert_eq!(ink.len(), 3, "{family:?} {size} x{scale}");
                let (smiley, keycap) = (ink[1], ink[2]);
                assert!(
                    smiley.is_color && keycap.is_color,
                    "{family:?} {size} x{scale}"
                );
                let top = |g: &Glyph| g.pos.y + g.uv_rect.offset.y;
                let bottom = |g: &Glyph| top(g) + g.uv_rect.size.y;
                assert!(
                    (top(smiley) - top(keycap)).abs() <= 1.5
                        && (bottom(smiley) - bottom(keycap)).abs() <= 1.5,
                    "{family:?} {size} x{scale}: 😀 {}..{}, #️⃣ {}..{}",
                    top(smiley),
                    bottom(smiley),
                    top(keycap),
                    bottom(keycap),
                );
                // The keycap takes the room of one emoji.
                assert!(
                    (keycap.advance_width - smiley.advance_width).abs() <= 1.0,
                    "{family:?} {size} x{scale}: {} {}",
                    keycap.advance_width,
                    smiley.advance_width
                );
            }
        }
    }
}
