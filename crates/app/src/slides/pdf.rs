//! A PDF drawn by Windows' own reader (Windows.Data.Pdf, the one Edge and
//! the Photos app use): exactly as it is, for presentations saved as PDF
//! from PowerPoint, Google Slides, Canva or Keynote. Measured here: 136 ms
//! to open, 46 ms a page at 1920 px wide. It runs on a thread with COM
//! started (render.rs), and waits for each of WinRT's operations there.

use std::path::Path;

use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::StorageFile;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows_core::{HRESULT, HSTRING};

use super::{MAX_H, MAX_W};

/// What Windows answers for a PDF with a password (ERROR_WRONG_PASSWORD).
const WRONG_PASSWORD: HRESULT = HRESULT(0x8007_052B_u32 as i32);

pub struct Pdf {
    doc: PdfDocument,
    count: usize,
    size: [u32; 2],
}

/// A page of `w` × `h` (in the reader's units) in pixels, as large as fits
/// the pictures' size.
fn fit(w: f32, h: f32) -> [u32; 2] {
    if !(w > 0.0 && h > 0.0) {
        return [MAX_W, MAX_H];
    }
    let scale = (MAX_W as f32 / w).min(MAX_H as f32 / h);
    [
        ((w * scale).round() as u32).max(1),
        ((h * scale).round() as u32).max(1),
    ]
}

impl Pdf {
    pub fn open(path: &Path) -> Result<Pdf, String> {
        let unreadable = |e: windows_core::Error| {
            crate::logln!("apresentação: pdf: {e}");
            "Não deu para abrir esse PDF.".to_owned()
        };
        // Windows' storage takes only a whole path, with backslashes and
        // no "..": anything else is a "bad path name".
        let path =
            std::path::absolute(path).map_err(|_| "Não deu para abrir esse PDF.".to_owned())?;
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))
            .and_then(|op| op.join())
            .map_err(unreadable)?;
        let doc = PdfDocument::LoadFromFileAsync(&file)
            .and_then(|op| op.join())
            .map_err(|e| {
                if e.code() == WRONG_PASSWORD {
                    "Esse PDF tem senha: salve uma cópia sem senha para apresentar.".to_owned()
                } else {
                    unreadable(e)
                }
            })?;
        let count = doc.PageCount().map_err(unreadable)? as usize;
        if count == 0 {
            return Err("Esse PDF não tem páginas.".into());
        }
        if count > super::wire::MAX_SLIDES {
            return Err(format!(
                "Esse PDF tem {count} páginas: o máximo é {}.",
                super::wire::MAX_SLIDES
            ));
        }
        let first = doc.GetPage(0).and_then(|p| p.Size()).map_err(unreadable)?;
        Ok(Pdf {
            doc,
            count,
            size: fit(first.Width, first.Height),
        })
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    /// Page `k` as a PNG (what the reader writes).
    pub fn draw(&self, k: usize) -> Result<Vec<u8>, String> {
        self.render(k).map_err(|e| e.to_string())
    }

    fn render(&self, k: usize) -> windows_core::Result<Vec<u8>> {
        let page = self.doc.GetPage(k as u32)?;
        let size = page.Size()?;
        let [w, h] = fit(size.Width, size.Height);
        let options = PdfPageRenderOptions::new()?;
        options.SetDestinationWidth(w)?;
        options.SetDestinationHeight(h)?;
        let stream = InMemoryRandomAccessStream::new()?;
        page.RenderWithOptionsToStreamAsync(&stream, &options)?
            .join()?;
        let len = u32::try_from(stream.Size()?).unwrap_or(u32::MAX);
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
        let loaded = reader.LoadAsync(len)?.join()?;
        let mut png = vec![0; loaded as usize];
        reader.ReadBytes(&mut png)?;
        Ok(png)
    }
}
