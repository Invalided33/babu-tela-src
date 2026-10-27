//! The tray icon is the app icon scaled down; with unread mentions or direct
//! messages it gets a red dot, like the earlier versions had.
//!
//! The icon lives on the thread that owns the window (it is not `Send`), and
//! its menu and clicks are delivered there too.

use std::cell::RefCell;
use std::sync::LazyLock;

use babutela_buildinfo::NAME;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::core::TrayAction;

pub const APP_ICON: &[u8] = include_bytes!("../../../build/appicon.png");

const SIZE: usize = 32;

thread_local! {
    static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

/// The app icon as RGBA, decoded once.
pub static APP_RGBA: LazyLock<Option<(Vec<u8>, usize, usize)>> = LazyLock::new(|| {
    let img = image::load_from_memory(APP_ICON).ok()?.into_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Some((img.into_raw(), w, h))
});

static ICONS: LazyLock<[Vec<u8>; 2]> = LazyLock::new(|| {
    let draw = |badge| match &*APP_RGBA {
        Some((rgba, w, h)) => scaled_icon(rgba, *w, *h, SIZE, badge),
        None => vec![0; SIZE * SIZE * 4],
    };
    [draw(false), draw(true)]
});

fn icon(badge: bool) -> Option<Icon> {
    Icon::from_rgba(ICONS[usize::from(badge)].clone(), SIZE as u32, SIZE as u32).ok()
}

/// Puts the icon in the tray. `on` receives the menu choices and clicks.
/// Must run on the window's thread, before anything else touches the tray.
pub fn start(on: impl Fn(TrayAction) + Send + Sync + 'static) -> Result<(), String> {
    let on = std::sync::Arc::new(on);
    // Both handlers can be set only once, and before the first event, or
    // the events go to a channel nobody reads.
    {
        let on = on.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let action = match e.id.0.as_str() {
                "open" => TrayAction::Open,
                "mute" => TrayAction::Mute,
                "deafen" => TrayAction::Deafen,
                "quit" => TrayAction::Quit,
                _ => return,
            };
            on(action);
        }));
    }
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => on(TrayAction::Open),
        _ => {}
    }));

    let menu = Menu::new();
    menu.append_items(&[
        &MenuItem::with_id("open", format!("Abrir {NAME}"), true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("mute", "Ligar/desligar microfone", true, None),
        &MenuItem::with_id("deafen", "Ligar/desligar áudio", true, None),
        &PredefinedMenuItem::separator(),
        // Not the predefined Quit item: that one ends the window's message
        // loop behind the app's back.
        &MenuItem::with_id("quit", "Sair", true, None),
    ])
    .map_err(|e| e.to_string())?;
    let mut builder = TrayIconBuilder::new()
        .with_id("main")
        .with_tooltip(NAME)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false);
    if let Some(i) = icon(false) {
        builder = builder.with_icon(i);
    }
    let tray = builder.build().map_err(|e| e.to_string())?;
    TRAY.with(|t| *t.borrow_mut() = Some(tray));
    Ok(())
}

/// Shows whether there is anything unread. Does nothing off the window's
/// thread or without a tray (tests, PROSA_NO_TRAY).
pub fn set_badge(n: u32) {
    TRAY.with(|t| {
        let Some(tray) = t.borrow().as_ref().cloned() else {
            return;
        };
        let _ = tray.set_icon(icon(n > 0));
        let tip = match n {
            0 => NAME.to_owned(),
            1 => format!("{NAME} — 1 mensagem nova"),
            n => format!("{NAME} — {n} mensagens novas"),
        };
        let _ = tray.set_tooltip(Some(tip));
    });
}

/// Takes the icon out of the tray, so it does not linger after the app
/// quits.
pub fn remove() {
    TRAY.with(|t| t.borrow_mut().take());
}

/// Whether there is a tray icon to come back from. Without one, hiding the
/// window would leave the app running with no way back.
pub fn present() -> bool {
    TRAY.with(|t| t.borrow().is_some())
}

/// Box-filters the RGBA app icon down to `size` and optionally draws the
/// unread dot (with a dark ring so it reads against the icon).
pub fn scaled_icon(src: &[u8], sw: usize, sh: usize, size: usize, badge: bool) -> Vec<u8> {
    let mut dst = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let (x0, x1) = (x * sw / size, (x + 1) * sw / size);
            let (y0, y1) = (y * sh / size, (y + 1) * sh / size);
            let (mut r, mut g, mut b, mut a, mut n) = (0f64, 0f64, 0f64, 0f64, 0f64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &src[(sy * sw + sx) * 4..][..4];
                    let alpha = f64::from(p[3]);
                    r += f64::from(p[0]) * alpha;
                    g += f64::from(p[1]) * alpha;
                    b += f64::from(p[2]) * alpha;
                    a += alpha;
                    n += 1.0;
                }
            }
            if a > 0.0 {
                dst[(y * size + x) * 4..][..4].copy_from_slice(&[
                    (r / a) as u8,
                    (g / a) as u8,
                    (b / a) as u8,
                    (a / n) as u8,
                ]);
            }
        }
    }
    if badge {
        let s = size as f64;
        let (cx, cy, rad) = (s * 0.76, s * 0.76, s * 0.24);
        for y in 0..size {
            for x in 0..size {
                let d = (x as f64 + 0.5 - cx).hypot(y as f64 + 0.5 - cy);
                let px = &mut dst[(y * size + x) * 4..][..4];
                blend(
                    px,
                    [0x0d, 0x0c, 0x1d],
                    (rad + s * 0.06 - d + 0.5).clamp(0.0, 1.0),
                );
                blend(px, [0xf2, 0x5f, 0x7a], (rad - d + 0.5).clamp(0.0, 1.0));
            }
        }
    }
    dst
}

fn blend(px: &mut [u8], c: [u8; 3], k: f64) {
    if k <= 0.0 {
        return;
    }
    let mix = |a: u8, b: u8| (f64::from(a) * k + f64::from(b) * (1.0 - k)) as u8;
    let alpha = f64::from(px[3]).max(k * 255.0) as u8;
    px.copy_from_slice(&[mix(c[0], px[0]), mix(c[1], px[1]), mix(c[2], px[2]), alpha]);
}

#[cfg(test)]
mod tests {
    #[test]
    fn icons_have_a_dot_only_with_unread() {
        let [plain, badge] = &*super::ICONS;
        assert_eq!(plain.len(), 32 * 32 * 4);
        assert!(
            plain.chunks(4).any(|p| p[3] > 0),
            "o ícone não devia ser transparente"
        );
        // The dot sits at the lower right.
        let at = |img: &Vec<u8>, x: usize, y: usize| img[(y * 32 + x) * 4..][..4].to_vec();
        assert_eq!(at(badge, 24, 24), vec![0xf2, 0x5f, 0x7a, 255]);
        assert_ne!(at(plain, 24, 24), at(badge, 24, 24));
    }

    #[test]
    fn badge_and_remove_are_harmless_without_a_tray() {
        super::set_badge(3);
        super::remove();
    }
}
