//! Pictures from the server (avatars and image attachments), loaded in the
//! background and kept as textures. Uploaded files never change (each name
//! is random), so a copy on disk is good forever; old copies are cleared
//! after a month without use.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use egui::{ColorImage, TextureHandle, TextureOptions, Vec2};
use sha2::{Digest, Sha256};

use crate::net::Api;

/// Copies on disk older than this are removed at startup.
const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 3600);

/// Pictures larger than this are not decoded (a decompression bomb would
/// otherwise eat the memory).
const MAX_PIXELS: u64 = 40_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fit {
    /// A square crop from the middle, `n` pixels wide: avatars.
    Cover(u32),
    /// The whole picture, no side longer than `n` pixels: attachments.
    Within(u32),
}

/// A loaded picture not drawn for this long lets its texture go (the copy
/// on disk brings it back in a moment): without it, every picture seen in
/// a long session stayed in memory.
const UNUSED_FOR: Duration = Duration::from_secs(5 * 60);

/// How often the unused ones are looked for.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

enum Slot {
    Loading,
    /// The texture, and when it was last asked for.
    Ready(TextureHandle, Instant),
    Failed,
}

struct Cache {
    map: HashMap<(String, Fit), Slot>,
    swept: Instant,
}

impl Cache {
    /// Lets go of the textures nobody asked for in `UNUSED_FOR`.
    fn sweep(&mut self, now: Instant) {
        if now.duration_since(self.swept) < SWEEP_EVERY {
            return;
        }
        self.swept = now;
        self.map.retain(|_, slot| match slot {
            Slot::Ready(_, used) => now.duration_since(*used) < UNUSED_FOR,
            Slot::Loading | Slot::Failed => true,
        });
    }
}

pub struct Images {
    ctx: egui::Context,
    rt: tokio::runtime::Handle,
    api: Arc<Api>,
    dir: PathBuf,
    cache: Mutex<Cache>,
}

impl Images {
    pub fn new(
        ctx: egui::Context,
        rt: tokio::runtime::Handle,
        api: Arc<Api>,
        dir: PathBuf,
    ) -> Arc<Images> {
        let _ = std::fs::create_dir_all(&dir);
        let images = Arc::new(Images {
            ctx,
            rt,
            api,
            dir,
            cache: Mutex::new(Cache {
                map: HashMap::new(),
                swept: Instant::now(),
            }),
        });
        let dir = images.dir.clone();
        images.rt.spawn_blocking(move || prune(&dir));
        images
    }

    /// The picture at `url`, or `None` while it loads (or if it failed).
    pub fn get(self: &Arc<Self>, url: &str, fit: Fit) -> Option<TextureHandle> {
        let key = (url.to_owned(), fit);
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.sweep(now);
        match cache.map.get_mut(&key) {
            Some(Slot::Ready(t, used)) => {
                *used = now;
                return Some(t.clone());
            }
            Some(Slot::Loading | Slot::Failed) => return None,
            None => {}
        }
        cache.map.insert(key.clone(), Slot::Loading);
        drop(cache);
        let me = self.clone();
        self.rt.spawn(async move {
            let slot = match me.load(&key.0, fit).await {
                Ok(img) => {
                    let name = format!("img:{}:{fit:?}", key.0);
                    let texture = me.ctx.load_texture(name, img, TextureOptions::LINEAR);
                    Slot::Ready(texture, Instant::now())
                }
                Err(e) => {
                    crate::logln!("imagem {}: {e}", key.0);
                    Slot::Failed
                }
            };
            me.cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .map
                .insert(key, slot);
            me.ctx.request_repaint();
        });
        None
    }

    /// The natural size of a loaded picture, for laying it out.
    pub fn size_of(texture: &TextureHandle) -> Vec2 {
        let [w, h] = texture.size();
        Vec2::new(w as f32, h as f32)
    }

    fn cache_path(&self, url: &str) -> PathBuf {
        let h = Sha256::digest(url.as_bytes());
        self.dir.join(hex::encode(&h[..16]))
    }

    async fn load(&self, url: &str, fit: Fit) -> Result<ColorImage, String> {
        let path = self.cache_path(url);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) if !b.is_empty() => {
                // Touching the file keeps a picture in use from being pruned.
                let p = path.clone();
                let _ = tokio::task::spawn_blocking(move || touch(&p)).await;
                b
            }
            _ => {
                let b = self.api.fetch_bytes(url, crate::net::PREVIEW_MAX).await?;
                let _ = tokio::fs::write(&path, &b).await;
                b
            }
        };
        // A message's picture is kept here as the server has it, encrypted
        // (e2e/file.rs), and decrypted only in memory.
        let bytes = crate::e2e::open_downloaded(url, bytes)?;
        tokio::task::spawn_blocking(move || decode(&bytes, fit))
            .await
            .map_err(|e| e.to_string())?
    }
}

/// Decodes and resizes a picture; GIFs show their first frame.
fn decode(bytes: &[u8], fit: Fit) -> Result<ColorImage, String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(format!("grande demais ({w}x{h})"));
    }
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let img = match fit {
        Fit::Cover(n) => {
            let side = img.width().min(img.height());
            let x = (img.width() - side) / 2;
            let y = (img.height() - side) / 2;
            let square = img.crop_imm(x, y, side, side);
            if side > n {
                square.resize_exact(n, n, image::imageops::FilterType::Triangle)
            } else {
                square
            }
        }
        Fit::Within(n) => {
            if img.width() > n || img.height() > n {
                img.resize(n, n, image::imageops::FilterType::Triangle)
            } else {
                img
            }
        }
    };
    let rgba = img.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

fn touch(path: &std::path::Path) {
    if let Ok(f) = std::fs::File::options().append(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Removes copies nobody has looked at for a month.
fn prune(dir: &std::path::Path) {
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
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn unused_pictures_let_their_texture_go() {
        let ctx = egui::Context::default();
        let t0 = Instant::now();
        let white = ColorImage::new([1, 1], vec![egui::Color32::WHITE]);
        let texture = ctx.load_texture("a", white, TextureOptions::LINEAR);
        let mut cache = Cache {
            map: HashMap::new(),
            swept: t0,
        };
        cache
            .map
            .insert(("a".into(), Fit::Cover(8)), Slot::Ready(texture, t0));
        cache.map.insert(("b".into(), Fit::Cover(8)), Slot::Loading);
        let at = |secs: u64| t0 + UNUSED_FOR + Duration::from_secs(secs);
        cache.sweep(t0 + UNUSED_FOR - Duration::from_secs(30));
        assert_eq!(cache.map.len(), 2, "used a little while ago");
        // Unused for long enough now, but the last look was 40 s ago.
        cache.sweep(at(10));
        assert_eq!(cache.map.len(), 2);
        cache.sweep(at(40));
        assert_eq!(cache.map.len(), 1, "only the one still loading stays");
    }

    #[test]
    fn avatars_are_cropped_square_and_shrunk() {
        let img = decode(&png(300, 200), Fit::Cover(64)).unwrap();
        assert_eq!(img.size, [64, 64]);
        // Small pictures are not blown up.
        let img = decode(&png(40, 30), Fit::Cover(64)).unwrap();
        assert_eq!(img.size, [30, 30]);
    }

    #[test]
    fn attachments_keep_their_shape() {
        let img = decode(&png(1600, 800), Fit::Within(800)).unwrap();
        assert_eq!(img.size, [800, 400]);
        let img = decode(&png(100, 50), Fit::Within(800)).unwrap();
        assert_eq!(img.size, [100, 50]);
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        assert!(decode(b"isto nao e uma imagem", Fit::Within(10)).is_err());
        assert!(decode(&[], Fit::Within(10)).is_err());
    }
}
