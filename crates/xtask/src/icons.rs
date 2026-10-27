//! `cargo xtask icones`: writes crates/app/assets/icons/*.svg, the strokes
//! the interface draws its icons from.
//!
//! The files come out with a white stroke: the app paints each icon in the
//! colour it wants by multiplying the image (egui::Image::tint), and
//! multiplying by white changes nothing. Run again only if a drawing changes.

use crate::tools::root;
use crate::{Result, fail};

/// 24x24 strokes, 2 wide: the interface's icons.
const ICONS: &[(&str, &str)] = &[
    (
        "hash",
        r#"<path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18"/>"#,
    ),
    (
        "speaker",
        r#"<path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="M15.5 8.5a5 5 0 0 1 0 7M19 5a10 10 0 0 1 0 14"/>"#,
    ),
    (
        "mic",
        r#"<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2M12 19v3"/>"#,
    ),
    (
        "mic-off",
        r#"<path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z"/><path d="M19 10v2a7 7 0 0 1-14 0v-2M12 19v3M3 3l18 18"/>"#,
    ),
    (
        "head",
        r#"<path d="M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 18 0v7a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3"/>"#,
    ),
    (
        "head-off",
        r#"<path d="M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 18 0v7a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3M3 3l18 18"/>"#,
    ),
    (
        "video",
        r#"<path d="m22 8-6 4 6 4V8Z"/><rect width="14" height="12" x="2" y="6" rx="2"/>"#,
    ),
    (
        "screen",
        r#"<rect width="20" height="14" x="2" y="3" rx="2"/><path d="M8 21h8M12 17v4"/>"#,
    ),
    (
        "hang",
        r#"<path d="M10.7 13.3a16 16 0 0 0 3.4 2.6l1.3-1.3a2 2 0 0 1 2.1-.4c.9.3 1.8.6 2.8.7a2 2 0 0 1 1.7 2v3a2 2 0 0 1-2.2 2A19.8 19.8 0 0 1 2 4.1 2 2 0 0 1 4.1 2h3a2 2 0 0 1 2 1.7c.1 1 .4 1.9.7 2.8a2 2 0 0 1-.5 2.1L8.1 9.9M22 2 2 22"/>"#,
    ),
    (
        "gear",
        concat!(
            r#"<circle cx="12" cy="12" r="3"/>"#,
            r#"<path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06"#,
            r#"a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09"#,
            r#"A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06"#,
            r#"a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09"#,
            r#"A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06"#,
            r#"a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09"#,
            r#"a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06"#,
            r#"a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09"#,
            r#"a1.65 1.65 0 0 0-1.51 1z"/>"#,
        ),
    ),
    ("plus", r#"<path d="M12 5v14M5 12h14"/>"#),
    ("x", r#"<path d="M18 6 6 18M6 6l12 12"/>"#),
    (
        "reply",
        r#"<path d="M9 14 4 9l5-5"/><path d="M20 20v-7a4 4 0 0 0-4-4H4"/>"#,
    ),
    (
        "edit",
        r#"<path d="M17 3a2.83 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/>"#,
    ),
    (
        "trash",
        r#"<path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#,
    ),
    (
        "smile",
        r#"<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01"/>"#,
    ),
    (
        "clip",
        r#"<path d="m21.4 11-9.2 9.2a6 6 0 0 1-8.5-8.5l8.6-8.5A4 4 0 1 1 18 8.8l-8.6 8.6a2 2 0 0 1-2.8-2.8l8.5-8.5"/>"#,
    ),
    (
        "users",
        r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8"/>"#,
    ),
    ("down", r#"<path d="m6 9 6 6 6-6"/>"#),
    (
        "chat",
        r#"<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>"#,
    ),
    (
        "file",
        r#"<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/>"#,
    ),
    (
        "flag",
        r#"<path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z"/><path d="M4 22v-7"/>"#,
    ),
    ("play", r##"<path d="m8 5 11 7-11 7V5z" fill="#fff"/>"##),
    (
        "pause",
        r##"<rect x="6" y="5" width="4" height="14" rx="1" fill="#fff"/><rect x="14" y="5" width="4" height="14" rx="1" fill="#fff"/>"##,
    ),
    (
        "monitor",
        r#"<rect width="20" height="14" x="2" y="3" rx="2"/><path d="M8 21h8M12 17v4M7 8h10M7 12h6"/>"#,
    ),
    (
        "window",
        r#"<rect width="20" height="16" x="2" y="4" rx="2"/><path d="M2 9h20M6 6.5h.01M9 6.5h.01"/>"#,
    ),
    (
        "volume",
        r#"<path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/>"#,
    ),
    (
        "volume-off",
        r#"<path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="m22 9-6 6M16 9l6 6"/>"#,
    ),
    (
        "expand",
        r#"<path d="M15 3h6v6M9 21H3v-6M21 3l-7 7M3 21l7-7"/>"#,
    ),
    (
        "sticker",
        r#"<path d="M15.5 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V8.5z"/><path d="M15 3v4a2 2 0 0 0 2 2h4M8 13h.01M16 13h.01"/><path d="M10 16s.8 1 2 1c1.3 0 2-1 2-1"/>"#,
    ),
    (
        "star",
        r#"<path d="m12 2 3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01z"/>"#,
    ),
    (
        "star-on",
        r##"<path d="m12 2 3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01z" fill="#fff"/>"##,
    ),
    ("left", r#"<path d="m15 18-6-6 6-6"/>"#),
    ("right", r#"<path d="m9 18 6-6-6-6"/>"#),
    (
        "presentation",
        r#"<path d="M2 3h20M21 3v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V3M7 21l5-5 5 5"/>"#,
    ),
    (
        "zoom-in",
        r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3M11 8v6M8 11h6"/>"#,
    ),
    (
        "zoom-out",
        r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3M8 11h6"/>"#,
    ),
    (
        "clock",
        r#"<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>"#,
    ),
    // The annotation tools (ink/): disappearing ink, the highlighter, the
    // shapes, the eraser, undo and redo, and saving the picture.
    (
        "vanish",
        r#"<path d="M3 20c4-1 7-4 9-8"/><path d="M15 8.5h.01M18 5.5h.01M21 3h.01"/>"#,
    ),
    (
        "marker",
        r#"<path d="m14.5 4.5 5 5-8 8h-5v-5z"/><path d="M4 21h8"/>"#,
    ),
    ("arrow", r#"<path d="M5 19 19 5"/><path d="M9 5h10v10"/>"#),
    (
        "square",
        r#"<rect width="16" height="16" x="4" y="4" rx="2"/>"#,
    ),
    ("circle", r#"<ellipse cx="12" cy="12" rx="9" ry="7.5"/>"#),
    (
        "eraser",
        r#"<path d="M9 20h11"/><path d="m4.5 15.5 9.8-9.8a1.5 1.5 0 0 1 2.1 0l3.4 3.4a1.5 1.5 0 0 1 0 2.1L11 20H8.9l-4.4-4.4a.9.9 0 0 1 0-1.3z"/><path d="m8.5 11.5 5 5"/>"#,
    ),
    (
        "undo",
        r#"<path d="M9 14 4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 0 10h-3"/>"#,
    ),
    (
        "redo",
        r#"<path d="m15 14 5-5-5-5"/><path d="M20 9H9a5 5 0 0 0 0 10h3"/>"#,
    ),
    (
        "save",
        r#"<path d="M12 4v11"/><path d="m7 10 5 5 5-5"/><path d="M5 20h14"/>"#,
    ),
    // Back to the mouse, on the toolbar over a shared screen.
    (
        "pointer",
        r#"<path d="M5 3l14 8-6 1.5L10.5 19z"/><path d="m13.5 13 4 6"/>"#,
    ),
    // Only the person sharing may annotate: the others, struck through.
    (
        "users-off",
        r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8M3 3l18 18"/>"#,
    ),
    // A file sent directly ("Enviar direto"), and the folder it went to.
    (
        "direct",
        r#"<path d="M14 2 5 13.5h6.5L10 22l9-11.5h-6.5z"/>"#,
    ),
    (
        "folder",
        r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2z"/>"#,
    ),
];

/// The title bar's three buttons: a 12 box and a thin stroke, as they were.
const WINDOW_ICONS: &[(&str, &str)] = &[
    ("win-min", r#"<path d="M2 6h8"/>"#),
    ("win-max", r#"<rect x="2.5" y="2.5" width="7" height="7"/>"#),
    ("win-close", r#"<path d="M2.5 2.5l7 7M9.5 2.5l-7 7"/>"#),
];

fn svg(body: &str, size: u32, width: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" fill=\"none\" \
         stroke=\"#ffffff\" stroke-width=\"{width}\" stroke-linecap=\"round\" \
         stroke-linejoin=\"round\">{body}</svg>\n"
    )
}

pub fn run(args: &[String]) -> Result {
    if let Some(extra) = args.first() {
        fail!("opção desconhecida: {extra}");
    }
    let out = root()
        .join("crates")
        .join("app")
        .join("assets")
        .join("icons");
    std::fs::create_dir_all(&out)?;
    for (name, body) in ICONS {
        std::fs::write(out.join(format!("{name}.svg")), svg(body, 24, "2"))?;
    }
    for (name, body) in WINDOW_ICONS {
        std::fs::write(out.join(format!("{name}.svg")), svg(body, 12, "1.2"))?;
    }
    println!(
        "{} ícones gravados em {}",
        ICONS.len() + WINDOW_ICONS.len(),
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files in the repository are exactly what this writes, so nobody
    /// edits an .svg by hand and loses it the next time this runs. New
    /// icons only need `cargo xtask icones`.
    #[test]
    fn the_icons_on_disk_match() {
        let dir = root()
            .join("crates")
            .join("app")
            .join("assets")
            .join("icons");
        for (name, body, size, width) in ICONS
            .iter()
            .map(|(n, b)| (n, b, 24, "2"))
            .chain(WINDOW_ICONS.iter().map(|(n, b)| (n, b, 12, "1.2")))
        {
            let path = dir.join(format!("{name}.svg"));
            let Ok(on_disk) = std::fs::read_to_string(&path) else {
                continue; // not written yet
            };
            assert_eq!(on_disk, svg(body, size, width), "{name}");
        }
    }
}
