//! The line drawings the interface uses, the same ones the old page drew
//! inline. They are stroked in white and painted by multiplying, so one file
//! serves every colour (`cargo xtask icones` writes them).

use egui::{Color32, Image, ImageSource, Vec2, include_image};

/// The size the stylesheet gave `svg.i`.
pub const SIZE: f32 = 18.0;

/// The smaller one used inside lists and reply lines.
pub const SMALL: f32 = 14.0;

/// The size on the round buttons under a call.
pub const LARGE: f32 = 22.0;

/// The title bar's own, thinner drawings.
pub const WINDOW: f32 = 12.0;

macro_rules! icons {
    ($($variant:ident => $file:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Icon {
            $($variant,)+
        }

        impl Icon {
            /// Every drawing, for the test that checks they are all real.
            #[cfg(test)]
            pub const ALL: &[Icon] = &[$(Icon::$variant,)+];

            fn source(self) -> ImageSource<'static> {
                match self {
                    $(Icon::$variant => {
                        include_image!(concat!("../assets/icons/", $file, ".svg"))
                    })+
                }
            }
        }
    };
}

icons! {
    Arrow => "arrow",
    Chat => "chat",
    Circle => "circle",
    Clip => "clip",
    Clock => "clock",
    Direct => "direct",
    Down => "down",
    Edit => "edit",
    Eraser => "eraser",
    Expand => "expand",
    File => "file",
    Flag => "flag",
    Folder => "folder",
    Gear => "gear",
    Hang => "hang",
    Hash => "hash",
    Head => "head",
    HeadOff => "head-off",
    Left => "left",
    Marker => "marker",
    Mic => "mic",
    MicOff => "mic-off",
    Monitor => "monitor",
    Pause => "pause",
    Play => "play",
    Plus => "plus",
    Presentation => "presentation",
    Redo => "redo",
    Reply => "reply",
    Right => "right",
    Save => "save",
    Screen => "screen",
    Smile => "smile",
    Speaker => "speaker",
    Square => "square",
    Star => "star",
    StarOn => "star-on",
    Sticker => "sticker",
    Text => "text",
    Trash => "trash",
    Undo => "undo",
    Users => "users",
    UsersOff => "users-off",
    Vanish => "vanish",
    Video => "video",
    Volume => "volume",
    VolumeOff => "volume-off",
    WinClose => "win-close",
    WinMax => "win-max",
    WinMin => "win-min",
    Window => "window",
    X => "x",
    ZoomIn => "zoom-in",
    ZoomOut => "zoom-out",
}

impl Icon {
    /// The drawing at a given size and colour, ready to add to a layout.
    pub fn image(self, size: f32, color: Color32) -> Image<'static> {
        Image::new(self.source())
            .fit_to_exact_size(Vec2::splat(size))
            .tint(color)
    }

    /// The drawing at the usual size.
    pub fn tinted(self, color: Color32) -> Image<'static> {
        self.image(SIZE, color)
    }

    /// The icon for a channel in the list: a hash for text, a speaker for voice.
    pub fn for_channel(kind: crate::model::ChannelKind) -> Icon {
        match kind {
            crate::model::ChannelKind::Voice => Icon::Speaker,
            _ => Icon::Hash,
        }
    }

    /// What a microphone button should show.
    pub fn mic(off: bool) -> Icon {
        if off { Icon::MicOff } else { Icon::Mic }
    }

    /// What a headphones button should show.
    pub fn head(off: bool) -> Icon {
        if off { Icon::HeadOff } else { Icon::Head }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every drawing has to be real SVG, or it would silently not appear.
    #[test]
    fn every_icon_is_embedded_svg() {
        assert!(Icon::ALL.len() >= 33);
        for icon in Icon::ALL {
            let ImageSource::Bytes { uri, bytes } = icon.source() else {
                panic!("{icon:?} não está embutido");
            };
            assert!(uri.ends_with(".svg"), "{icon:?}: {uri}");
            let text = std::str::from_utf8(&bytes).expect("utf-8");
            assert!(text.starts_with("<svg"), "{icon:?} não começa com <svg");
            assert!(
                text.contains("stroke=\"#ffffff\""),
                "{icon:?} sem o traço branco"
            );
        }
    }

    #[test]
    fn toggles_pick_the_crossed_out_drawing() {
        assert_eq!(Icon::mic(false), Icon::Mic);
        assert_eq!(Icon::mic(true), Icon::MicOff);
        assert_eq!(Icon::head(true), Icon::HeadOff);
        use crate::model::ChannelKind;
        assert_eq!(Icon::for_channel(ChannelKind::Voice), Icon::Speaker);
        assert_eq!(Icon::for_channel(ChannelKind::Text), Icon::Hash);
        assert_eq!(Icon::for_channel(ChannelKind::Dm), Icon::Hash);
    }
}
