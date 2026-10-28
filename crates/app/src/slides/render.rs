//! Drawing my presentation, on a thread of its own and below the others'
//! priority: the file opens (a .pptx with BetterOffice, a PDF with
//! Windows' reader), then the slide on screen is drawn first and the rest
//! nearest it, ahead first, so the next one is ready before it is needed.
//! Each picture goes into `Slides`, for the call to send and the
//! presenter's view to show.

use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::Slides;
use super::pdf::Pdf;
use super::pptx::Pptx;

/// The largest file presented: BetterOffice holds all of a .pptx in memory
/// while it draws (its videos too).
const MAX_FILE: u64 = 300 * 1024 * 1024;

/// A picture past this size goes as a JPEG when that is much smaller: a
/// photo, which as a PNG took up to ten times as much (measured on
/// PowerPoint's templates). Text and drawings stay PNG, exact.
const JPEG_ABOVE: usize = 200 * 1024;
const JPEG_QUALITY: u8 = 88;

enum Doc {
    Pptx(Box<Pptx>),
    Pdf(Pdf),
}

impl Doc {
    fn count(&self) -> usize {
        match self {
            Doc::Pptx(d) => d.count(),
            Doc::Pdf(d) => d.count(),
        }
    }

    /// How many images slide `k` produces (>1 when it has build steps).
    fn step_count(&self, k: usize) -> usize {
        match self {
            Doc::Pptx(d) => d.step_count(k),
            Doc::Pdf(_) => 1,
        }
    }

    fn size(&self) -> [u32; 2] {
        match self {
            Doc::Pptx(d) => d.size(),
            Doc::Pdf(d) => d.size(),
        }
    }

    fn notes(&self) -> Vec<String> {
        match self {
            Doc::Pptx(d) => d.notes(),
            Doc::Pdf(d) => vec![String::new(); d.count()],
        }
    }

    /// Slide `k` as a PNG: the final state (all shapes visible).
    fn draw(&self, k: usize) -> Result<Vec<u8>, String> {
        match self {
            Doc::Pptx(d) => d.draw(k),
            Doc::Pdf(d) => d.draw(k),
        }
    }

    /// Slide `k` at build step `step` (for slides with entrance
    /// animations). Falls back to `draw` for PDFs or slides without steps.
    fn draw_step(&self, k: usize, step: usize) -> Result<Vec<u8>, String> {
        match self {
            Doc::Pptx(d) => d.draw_step(k, step),
            Doc::Pdf(d) => d.draw(k),
        }
    }
}

/// Opens and draws `path` as presentation `id`. What goes wrong is said
/// through `toast`, and the presentation stops.
pub fn start(
    path: PathBuf,
    id: u32,
    slides: Arc<Slides>,
    toast: Arc<dyn Fn(String) + Send + Sync>,
) {
    let spawned = std::thread::Builder::new()
        .name("apresentacao".into())
        .spawn({
            let (slides, toast) = (slides.clone(), toast.clone());
            move || {
                lower_priority();
                if let Err(e) = run(&path, id, &slides) {
                    slides.stop_mine_if(id);
                    toast(e);
                }
            }
        });
    if spawned.is_err() {
        slides.stop_mine_if(id);
        toast(crate::t!("slides.present_failed").into());
    }
}

/// COM on this thread, for Windows' PDF reader, until dropped.
struct Com(bool);

impl Com {
    fn start() -> Com {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        // SAFETY: started on this thread, and stopped on it by Drop (only
        // when it started here).
        Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: this thread started COM (see `start`).
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Drawing is work for the background: the call's sound and video, and
/// the window, go first.
fn lower_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    // SAFETY: the pseudo-handle of this very thread, valid while it runs.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
}

fn run(path: &Path, id: u32, slides: &Slides) -> Result<(), String> {
    let opening = Instant::now();
    let unreadable = || "Não deu para abrir esse arquivo.".to_owned();
    let mut head = [0u8; 8];
    {
        let mut file = std::fs::File::open(path).map_err(|_| unreadable())?;
        if file.metadata().map_or(0, |m| m.len()) > MAX_FILE {
            return Err("Esse arquivo é grande demais para apresentar (o máximo é 300 MB).".into());
        }
        file.read(&mut head).map_err(|_| unreadable())?;
    }
    let pdf = head.starts_with(b"%PDF");
    // Kept until the end: the PDF reader's pages need it while they draw.
    let _com = pdf.then(Com::start);
    let doc = if pdf {
        Doc::Pdf(Pdf::open(path)?)
    } else if head.starts_with(b"PK\x03\x04") {
        let bytes = std::fs::read(path).map_err(|_| unreadable())?;
        Doc::Pptx(Box::new(Pptx::open(&bytes)?))
    } else if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        // The old binary .ppt. If PowerPoint is installed, it converts to
        // .pptx invisibly (the user never sees it open); otherwise the
        // person is told to save it themselves.
        let _com = Com::start();
        match ppt_to_pptx(path) {
            Some(converted) => {
                let bytes = std::fs::read(&converted).map_err(|_| unreadable())?;
                let _ = std::fs::remove_file(&converted);
                Doc::Pptx(Box::new(Pptx::open(&bytes)?))
            }
            None => {
                return Err(
                    "Esse arquivo é do PowerPoint antigo (.ppt). O PowerPoint não está \
                     instalado para converter: salve como .pptx ou PDF para apresentar."
                        .into(),
                );
            }
        }
    } else {
        return Err("Só dá para apresentar arquivos do PowerPoint (.pptx) ou PDF.".into());
    };
    crate::logln!(
        "apresentação: {} slides de {}x{}, aberta em {} ms",
        doc.count(),
        doc.size()[0],
        doc.size()[1],
        opening.elapsed().as_millis()
    );
    // With build steps, one slide becomes several images (one per click).
    // The images are numbered sequentially; `notes_for` maps each image
    // back to its slide's notes.
    let mut image_count = 0;
    let mut notes = Vec::new();
    let mut image_to_slide: Vec<(usize, usize)> = Vec::new(); // (slide, step)
    for k in 0..doc.count() {
        let steps = doc.step_count(k);
        let slide_notes = doc.notes().into_iter().nth(k).unwrap_or_default();
        for step in 0..steps {
            image_to_slide.push((k, step));
            notes.push(slide_notes.clone());
        }
        image_count += steps;
    }
    if image_count != doc.count() {
        crate::logln!(
            "apresentação: {} slides viram {} imagens (passos de clique)",
            doc.count(),
            image_count
        );
    }
    if !slides.opened_mine(id, image_count, doc.size(), notes) {
        return Ok(());
    }
    let drawing = Instant::now();
    let mut drawn = 0;
    while let Some(image_idx) = slides.next_to_draw(id) {
        let Some(&(k, step)) = image_to_slide.get(image_idx) else {
            break;
        };
        let started = Instant::now();
        let total_steps = doc.step_count(k);
        // The final step (or the only one) uses the fast path.
        let made = if total_steps == 1 || step + 1 == total_steps {
            std::panic::catch_unwind(AssertUnwindSafe(|| doc.draw(k)))
        } else {
            std::panic::catch_unwind(AssertUnwindSafe(|| doc.draw_step(k, step)))
        }
        .unwrap_or_else(|_| Err("o desenho parou no meio".into()))
        .and_then(for_the_wire);
        let (picture, failed) = match made {
            Ok(picture) => (picture, false),
            Err(e) => {
                crate::logln!("apresentação: slide {} passo {}: {e}", k + 1, step);
                (placeholder(doc.size()), true)
            }
        };
        crate::probe::since("slides.desenho", started);
        slides.drawn_mine(id, image_idx, picture, failed);
        drawn += 1;
    }
    if drawn > 0 {
        crate::logln!(
            "apresentação: {drawn} imagens desenhadas em {} ms",
            drawing.elapsed().as_millis()
        );
    }
    slides.finished_mine(id);
    Ok(())
}

/// The picture as it goes out: the PNG, or a JPEG of it when that is less
/// than two thirds of it.
fn for_the_wire(png: Vec<u8>) -> Result<Vec<u8>, String> {
    if png.len() <= JPEG_ABOVE {
        return Ok(png);
    }
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?
        .into_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&img)
        .map_err(|e| e.to_string())?;
    Ok(if png.len() > jpeg.len() * 3 / 2 {
        jpeg
    } else {
        png
    })
}

/// Converts a .ppt to a temporary .pptx using PowerPoint (if installed).
/// Uses a small PowerShell script that calls PowerPoint through COM, so the
/// conversion works invisibly without opening a window. Returns `None`
/// when PowerPoint is not installed or the conversion fails.
fn ppt_to_pptx(path: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(path).ok()?;
    let out = std::env::temp_dir().join(format!(
        "babutela-ppt-{}.pptx",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    ));
    // PowerShell 5.1 (always present on Windows 10+) drives PowerPoint's
    // COM, hidden (WithWindow = false), and saves as .pptx (format 24).
    let script = format!(
        "$ErrorActionPreference='Stop'\n\
         $app = New-Object -ComObject PowerPoint.Application\n\
         $p = $app.Presentations.Open('{}', -1, 0, 0)\n\
         $p.SaveAs('{}', 24)\n\
         $p.Close()\n\
         if ($app.Presentations.Count -eq 0) {{ $app.Quit() }}\n\
         [Runtime.InteropServices.Marshal]::ReleaseComObject($app) | Out-Null",
        abs.to_string_lossy().replace('\'', "''"),
        out.to_string_lossy().replace('\'', "''"),
    );
    let result = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    match result {
        Ok(output) if output.status.success() && out.is_file() => {
            crate::logln!("apresentação: .ppt convertido para .pptx pelo PowerPoint");
            Some(out)
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            crate::logln!("apresentação: PowerPoint não converteu o .ppt: {stderr}");
            None
        }
        Err(e) => {
            crate::logln!("apresentação: não deu para chamar o PowerShell: {e}");
            None
        }
    }
}

/// A plain grey picture of the slide's shape, for a slide that could not
/// be drawn (an eighth of its size: it only has to fill the room).
fn placeholder(size: [u32; 2]) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(
        (size[0] / 8).max(1),
        (size[1] / 8).max(1),
        image::Rgb([0xd9, 0xd9, 0xde]),
    );
    let mut png = std::io::Cursor::new(Vec::new());
    let _ = img.write_to(&mut png, image::ImageFormat::Png);
    png.into_inner()
}
