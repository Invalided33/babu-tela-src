//! The look of the app: the same indigo palette, type sizes and corner radii
//! the stylesheet used, now set straight on egui.

use std::sync::Arc;

use egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Visuals,
};

/// Indigo surfaces and a periwinkle-to-blue accent, taken from the app icon.
pub const BG0: Color32 = Color32::from_rgb(0x0d, 0x0c, 0x1d);
pub const BG1: Color32 = Color32::from_rgb(0x14, 0x13, 0x30);
pub const BG2: Color32 = Color32::from_rgb(0x1a, 0x19, 0x39);
pub const BG3: Color32 = Color32::from_rgb(0x25, 0x24, 0x50);
pub const BG4: Color32 = Color32::from_rgb(0x34, 0x32, 0x67);

pub const TEXT: Color32 = Color32::from_rgb(0xec, 0xeb, 0xff);
pub const MUTED: Color32 = Color32::from_rgb(0xa3, 0xa2, 0xc9);
pub const FAINT: Color32 = Color32::from_rgb(0x71, 0x6f, 0x9f);

pub const ACCENT: Color32 = Color32::from_rgb(0x85, 0x90, 0xff);
pub const ACCENT_2: Color32 = Color32::from_rgb(0x57, 0x63, 0xee);
pub const ACCENT_INK: Color32 = Color32::from_rgb(0x0d, 0x0c, 0x26);
/// The accent at 16% opacity, for mentions and the highlight behind them.
pub const ACCENT_SOFT: Color32 = Color32::from_rgba_premultiplied(0x15, 0x17, 0x29, 0x29);

/// The two ends of the gradient the buttons used.
pub const GRAD_A: Color32 = Color32::from_rgb(0xa6, 0xb0, 0xff);
pub const GRAD_B: Color32 = Color32::from_rgb(0x6c, 0x78, 0xf4);

pub const ONLINE: Color32 = Color32::from_rgb(0x43, 0xd3, 0x9e);
pub const DANGER: Color32 = Color32::from_rgb(0xf2, 0x5f, 0x7a);
pub const WARN: Color32 = Color32::from_rgb(0xf5, 0xb9, 0x5c);

/// The ink on a warning strip, which is a light background.
pub const WARN_INK: Color32 = Color32::from_rgb(0x2b, 0x1d, 0x00);

pub const RADIUS: u8 = 10;
pub const RADIUS_SM: u8 = 6;
pub const RADIUS_MD: u8 = 8;

/// The fixed widths the layout used.
pub const RAIL_W: f32 = 68.0;
pub const SIDEBAR_W: f32 = 244.0;
pub const MEMBERS_W: f32 = 232.0;
pub const HEAD_H: f32 = 50.0;
pub const TITLEBAR_H: f32 = 32.0;

/// Below this the member list steps aside.
pub const NO_MEMBERS: f32 = 1000.0;

// ---------------------------------------------------------------- type

/// The family used for anything that was bold in the stylesheet. egui has no
/// synthetic bold, so the semibold face is its own family.
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn body() -> FontId {
    FontId::proportional(14.0)
}

pub fn small() -> FontId {
    FontId::proportional(13.0)
}

pub fn tiny() -> FontId {
    FontId::proportional(11.0)
}

pub fn mono() -> FontId {
    FontId::monospace(13.0)
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, bold_family())
}

/// A heading inside a dialog.
pub fn h3() -> FontId {
    bold(18.0)
}

/// The heading on the sign-in card and at the start of a conversation.
pub fn h1() -> FontId {
    bold(22.0)
}

/// Where Windows keeps the faces the stylesheet asked for. Missing files are
/// skipped, and egui's own fonts carry on.
const FACES: &[(&str, &str)] = &[
    ("ui", r"C:\Windows\Fonts\segoeui.ttf"),
    ("bold", r"C:\Windows\Fonts\seguisb.ttf"),
    ("mono", r"C:\Windows\Fonts\consola.ttf"),
    // Stars, arrows and the like, which Segoe UI itself does not have.
    ("symbol", r"C:\Windows\Fonts\seguisym.ttf"),
    ("emoji", r"C:\Windows\Fonts\seguiemj.ttf"),
];

/// Country flags in colour, which Segoe UI Emoji leaves out on purpose (it
/// draws "BR" for 🇧🇷). "Twemoji Country Flags": the flags of Twemoji (© X
/// Corp., CC-BY 4.0) cut out of Mozilla's Twemoji font by talkjs
/// (github.com/talkjs/country-flag-emoji-polyfill, MIT). It has only the
/// regional indicator letters and the flags they form, so it is safe to put
/// ahead of the other faces. build.rs unpacks it from WOFF2.
const FLAGS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/flags.ttf"));

/// One font file as it was read: its path, size and when it last changed
/// (in nanoseconds since 1970).
pub(crate) type FileStamp = (&'static str, u64, u128);

/// The faces read from Windows' folder, once: egui, the emoji fonts
/// (emoji_font.rs) and the emoji list's drawing check (emoji.rs) share the
/// same bytes. Segoe UI Emoji alone is 13 MB.
struct Loaded {
    defs: FontDefinitions,
    stamp: Vec<FileStamp>,
}

fn loaded() -> &'static Loaded {
    static LOADED: std::sync::OnceLock<Loaded> = std::sync::OnceLock::new();
    LOADED.get_or_init(read_fonts)
}

/// The app's fonts: for egui, and for the copies of them that lay emoji
/// out on their own (emoji_font.rs).
pub(crate) fn font_definitions() -> FontDefinitions {
    loaded().defs.clone()
}

/// The font files the fonts were read from, as they were then, for emoji.rs
/// to know when what they draw may have changed (Windows updates its emoji
/// font now and then).
pub(crate) fn font_stamp() -> &'static [FileStamp] {
    &loaded().stamp
}

fn read_fonts() -> Loaded {
    let mut defs = FontDefinitions::default();
    defs.font_data
        .insert("flags".to_owned(), Arc::new(FontData::from_static(FLAGS)));
    let mut loaded = vec!["flags"];
    let mut stamp = Vec::new();
    for (name, path) in FACES {
        let changed = std::fs::metadata(path).ok().map(|m| {
            let since = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .unwrap_or_default();
            (m.len(), since.as_nanos())
        });
        if let Ok(bytes) = std::fs::read(path) {
            let (len, since) = changed.unwrap_or((bytes.len() as u64, 0));
            stamp.push((*path, len, since));
            defs.font_data
                .insert((*name).to_owned(), Arc::new(FontData::from_owned(bytes)));
            loaded.push(*name);
        }
    }
    let has = |name: &str| loaded.contains(&name);
    // egui picks the face for each character by walking the chain and
    // taking the first one that has it. Segoe UI Symbol carries plain
    // outlines for most pictographs too, so Segoe UI Emoji (whose glyphs are
    // COLR layers, drawn in colour with epaint's `color_fonts`) has to come
    // before it, or every emoji comes out in one colour. Segoe UI stays first:
    // letters, digits, © and the arrows keep their text shapes. The flags go
    // before Segoe UI Emoji, which has the letters but not the flags.
    let mut front = |family: &FontFamily, names: &[&str]| {
        if let Some(list) = defs.families.get_mut(family) {
            for name in names.iter().rev() {
                if has(name) {
                    list.insert(0, (*name).to_owned());
                }
            }
        }
    };
    front(
        &FontFamily::Proportional,
        &["ui", "flags", "emoji", "symbol"],
    );
    front(&FontFamily::Monospace, &["mono"]);
    // Code blocks fall back to the same faces, so an emoji pasted into one
    // still draws.
    if let Some(list) = defs.families.get_mut(&FontFamily::Monospace) {
        list.extend(
            ["flags", "emoji", "symbol"]
                .into_iter()
                .filter(|n| has(n))
                .map(str::to_owned),
        );
    }
    // Bold falls through to the regular face and then to the emoji fonts, so
    // a bold name with an emoji in it still draws.
    let mut chain = Vec::new();
    for name in ["bold", "ui", "flags", "emoji", "symbol"] {
        if has(name) {
            chain.push(name.to_owned());
        }
    }
    for name in defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default()
    {
        if !chain.contains(&name) {
            chain.push(name);
        }
    }
    defs.families.insert(bold_family(), chain);
    Loaded { defs, stamp }
}

// ---------------------------------------------------------------- setup

/// Applies the palette, the fonts and the spacing. Called once at startup.
pub fn apply(ctx: &Context) {
    ctx.set_fonts(font_definitions());
    // Emoji the chain above would draw in pieces, in black and white or not
    // at all (emoji_font.rs).
    ctx.add_glyph_rasterizer(crate::emoji_font::rasterizer());
    // The app has one look; Windows switching to a light theme must not
    // repaint it in colours the rest of the interface does not expect.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.visuals = visuals();
        style.text_styles = [
            (TextStyle::Small, tiny()),
            (TextStyle::Body, body()),
            (TextStyle::Button, body()),
            (TextStyle::Heading, h3()),
            (TextStyle::Monospace, mono()),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.window_margin = Margin::same(22);
        style.spacing.menu_margin = Margin::same(6);
        style.spacing.interact_size.y = 24.0;
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = false;
        style.spacing.scroll.foreground_color = false;
        style.spacing.menu_width = 210.0;
        // Nothing in this interface was ever underlined just for being a link.
        style.url_in_tooltip = false;
        style.wrap_mode = Some(egui::TextWrapMode::Wrap);
    });
}

fn visuals() -> Visuals {
    let mut visuals = Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = BG2;
    visuals.window_fill = BG2;
    visuals.extreme_bg_color = BG0;
    visuals.faint_bg_color = BG3;
    visuals.code_bg_color = BG0;
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = DANGER;
    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS);
    visuals.window_stroke = Stroke::new(1.0, BG4);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 10],
        blur: 30,
        spread: 0,
        color: Color32::from_black_alpha(0xa0),
    };
    visuals.popup_shadow = visuals.window_shadow;
    visuals.selection.bg_fill = ACCENT_2;
    visuals.selection.stroke = Stroke::new(1.0, TEXT);

    // Buttons and fields: flat surfaces that lift a shade on hover, which is
    // what the stylesheet did.
    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = BG2;
    w.noninteractive.weak_bg_fill = BG2;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BG3);
    w.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
    w.noninteractive.corner_radius = CornerRadius::same(RADIUS_MD);

    w.inactive.bg_fill = BG3;
    w.inactive.weak_bg_fill = BG3;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, MUTED);
    w.inactive.corner_radius = CornerRadius::same(RADIUS_MD);

    w.hovered.bg_fill = BG4;
    w.hovered.weak_bg_fill = BG4;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.corner_radius = CornerRadius::same(RADIUS_MD);
    w.hovered.expansion = 0.0;

    w.active.bg_fill = ACCENT_2;
    w.active.weak_bg_fill = ACCENT_2;
    w.active.bg_stroke = Stroke::NONE;
    w.active.fg_stroke = Stroke::new(1.0, TEXT);
    w.active.corner_radius = CornerRadius::same(RADIUS_MD);
    w.active.expansion = 0.0;

    w.open.bg_fill = BG4;
    w.open.weak_bg_fill = BG4;
    w.open.bg_stroke = Stroke::NONE;
    w.open.fg_stroke = Stroke::new(1.0, TEXT);
    w.open.corner_radius = CornerRadius::same(RADIUS_MD);

    visuals
}

// ---------------------------------------------------------------- colours

/// Reads a colour the server sent, like "#8590ff". Also takes the three
/// character form and tolerates a missing hash.
pub fn parse_hex(text: &str) -> Option<Color32> {
    let hex = text.trim().trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        3 => {
            let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
            Some(Color32::from_rgb(digit(0)?, digit(1)?, digit(2)?))
        }
        6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(
            byte(0)?,
            byte(2)?,
            byte(4)?,
            byte(6)?,
        )),
        _ => None,
    }
}

/// A colour the server sent, or `fallback` when it sent nothing usable.
pub fn color_or(text: &str, fallback: Color32) -> Color32 {
    parse_hex(text).unwrap_or(fallback)
}

/// The dot next to an avatar.
pub fn status_color(status: crate::model::Status) -> Color32 {
    use crate::model::Status;
    match status {
        Status::Online => ONLINE,
        Status::Idle => WARN,
        Status::Dnd => DANGER,
        Status::Invisible | Status::Offline => FAINT,
    }
}

/// Mixes two colours, for the gradient on the main buttons.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t) as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_colours_the_server_sends() {
        assert_eq!(parse_hex("#8590ff"), Some(ACCENT));
        assert_eq!(parse_hex("8590FF"), Some(ACCENT));
        assert_eq!(parse_hex("  #8590ff  "), Some(ACCENT));
        assert_eq!(parse_hex("#fff"), Some(Color32::WHITE));
        assert_eq!(parse_hex("#000"), Some(Color32::BLACK));
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("#zzzzzz"), None);
        assert_eq!(color_or("", ACCENT), ACCENT);
        assert_eq!(color_or("#43d39e", ACCENT), ONLINE);
    }

    #[test]
    fn the_status_dot_matches_the_stylesheet() {
        use crate::model::Status;
        assert_eq!(status_color(Status::Online), ONLINE);
        assert_eq!(status_color(Status::Idle), WARN);
        assert_eq!(status_color(Status::Dnd), DANGER);
        assert_eq!(status_color(Status::Invisible), FAINT);
        assert_eq!(status_color(Status::Offline), FAINT);
    }

    #[test]
    fn mixing_reaches_both_ends() {
        assert_eq!(mix(GRAD_A, GRAD_B, 0.0), GRAD_A);
        assert_eq!(mix(GRAD_A, GRAD_B, 1.0), GRAD_B);
        assert_eq!(mix(GRAD_A, GRAD_B, -5.0), GRAD_A);
        let middle = mix(Color32::BLACK, Color32::WHITE, 0.5);
        assert_eq!(middle, Color32::from_rgb(127, 127, 127));
    }

    /// Lays `text` out with the app's fonts and says, per character, whether
    /// its glyph keeps its own colours (instead of taking the text colour).
    fn colour_per_char(text: &str, font: FontId) -> Vec<(char, bool)> {
        let mut fonts =
            epaint::text::Fonts::new(epaint::text::TextOptions::default(), font_definitions());
        let galley =
            fonts
                .with_pixels_per_point(1.0)
                .layout_no_wrap(text.to_owned(), font, Color32::WHITE);
        galley.rows[0]
            .row
            .glyphs
            .iter()
            .map(|g| (g.chr, g.is_color))
            .collect()
    }

    #[test]
    fn emoji_are_drawn_in_colour() {
        // Only on a machine with the Windows faces (every Windows 10 and 11).
        if !std::path::Path::new(r"C:\Windows\Fonts\seguiemj.ttf").exists() {
            return;
        }
        for font in [body(), bold(14.0), mono()] {
            let glyphs = colour_per_char("a😀🔥❤\u{fe0f}👍🏽★→", font.clone());
            let colour: Vec<char> = glyphs.iter().filter(|g| g.1).map(|g| g.0).collect();
            // The heart with its emoji selector, and the thumb with its skin
            // tone, are one glyph each (the first character carries it).
            assert_eq!(colour, ['😀', '🔥', '❤', '👍'], "{font:?}: {glyphs:?}");
        }
    }

    /// The flag face goes ahead of the others, which is only harmless while
    /// it has nothing but flag parts: the regional indicator letters, the
    /// black flag and the tag characters of 🏴󠁧󠁢󠁥󠁮󠁧󠁿.
    #[test]
    fn the_flag_face_has_only_flags() {
        let mut defs = FontDefinitions::empty();
        defs.font_data
            .insert("flags".to_owned(), Arc::new(FontData::from_static(FLAGS)));
        defs.families
            .insert(FontFamily::Proportional, vec!["flags".to_owned()]);
        let mut fonts = epaint::text::Fonts::new(epaint::text::TextOptions::default(), defs);
        let mut view = fonts.with_pixels_per_point(1.0);
        let chars: Vec<char> = view
            .characters(&FontFamily::Proportional)
            .keys()
            .copied()
            .collect();
        assert!(chars.len() > 26, "{chars:?}");
        for c in chars {
            let flag_part =
                matches!(c, '\u{1F1E6}'..='\u{1F1FF}' | '\u{1F3F4}' | '\u{E0020}'..='\u{E007F}');
            assert!(flag_part, "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn flags_are_drawn_in_colour() {
        let glyphs = colour_per_char("a🇧🇷🇵🇹", body());
        // Each pair of letters is one flag, carried by its first letter.
        let colour: Vec<bool> = glyphs.iter().map(|g| g.1).collect();
        assert_eq!(colour, [false, true, false, true, false], "{glyphs:?}");
    }

    #[test]
    fn the_colour_face_comes_before_the_one_colour_face() {
        let defs = font_definitions();
        for family in [FontFamily::Proportional, bold_family()] {
            let chain = &defs.families[&family];
            let at = |name: &str| chain.iter().position(|n| n == name);
            if let (Some(emoji), Some(symbol)) = (at("emoji"), at("symbol")) {
                assert!(emoji < symbol, "{family:?}: {chain:?}");
            }
            let mut unique = chain.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), chain.len(), "{family:?}: {chain:?}");
        }
    }
}
