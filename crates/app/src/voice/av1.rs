//! AV1 from the others, decoded on the processor with rav1d (dav1d ported
//! to Rust, with its assembly), as openh264 decodes H.264 (video.rs). The
//! pictures come out in I420, which `i420_to_rgba` turns into egui's
//! pixels with the same colours the H.264 path gives them.

use std::ffi::c_int;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::DAV1D_PIXEL_LAYOUT_I420;
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_get_picture,
    dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

/// dav1d's "not now" (-EAGAIN; EAGAIN is 11 in the C runtime).
const EAGAIN: c_int = -11;

/// One person's AV1 decoder.
pub struct Decoder {
    context: Option<Dav1dContext>,
}

impl Decoder {
    pub fn new() -> Result<Decoder, String> {
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        // SAFETY: dav1d_default_settings writes every field of the settings
        // it is pointed at.
        let mut settings = unsafe {
            dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
            settings.assume_init()
        };
        // Live video: each picture comes out once it is decoded, none held
        // back for other frame threads; two threads share the work inside
        // one picture.
        settings.max_frame_delay = 1;
        settings.n_threads = 2;
        let mut context = None;
        // SAFETY: both pointers are to locals that outlive the call.
        let opened = unsafe {
            dav1d_open(
                Some(NonNull::from(&mut context)),
                Some(NonNull::from(&mut settings)),
            )
        };
        if opened.0 != 0 || context.is_none() {
            return Err(format!("o decodificador AV1 não abriu ({})", opened.0));
        }
        Ok(Decoder { context })
    }

    /// Decodes one temporal unit (OBUs with their sizes). The picture it
    /// shows, when there is one.
    pub fn decode(&mut self, unit: &[u8]) -> Result<Option<Picture>, String> {
        let context = self.context.ok_or("sem decodificador AV1")?;
        if unit.is_empty() {
            return Ok(None);
        }
        let mut data = Dav1dData::default();
        // SAFETY: dav1d allocates `unit.len()` bytes for `data` and hands
        // them over; they are all written before dav1d reads any.
        unsafe {
            let buf = dav1d_data_create(Some(NonNull::from(&mut data)), unit.len());
            if buf.is_null() {
                return Err("sem memória para o AV1".into());
            }
            std::ptr::copy_nonoverlapping(unit.as_ptr(), buf, unit.len());
        }
        let mut shown = None;
        let failed = |data: &mut Dav1dData, what: &str, code: c_int| {
            // SAFETY: what dav1d did not take of `data` is given back once.
            unsafe { dav1d_data_unref(Some(NonNull::from(data))) };
            Err(format!("{what} ({code})"))
        };
        loop {
            // SAFETY: the context is open, and `data` is dav1d's.
            let sent = unsafe { dav1d_send_data(Some(context), Some(NonNull::from(&mut data))) };
            if sent.0 != 0 && sent.0 != EAGAIN {
                return failed(&mut data, "AV1 inválido", sent.0);
            }
            let mut picture = Dav1dPicture::default();
            // SAFETY: the context is open; the picture is a local dav1d
            // writes to.
            let got =
                unsafe { dav1d_get_picture(Some(context), Some(NonNull::from(&mut picture))) };
            if got.0 == 0 {
                // A newer picture replaces one decoded before it.
                shown = Some(Picture(picture));
            } else if got.0 != EAGAIN {
                return failed(&mut data, "a imagem AV1 não saiu", got.0);
            }
            if data.sz == 0 {
                return Ok(shown);
            }
            if sent.0 == EAGAIN && got.0 == EAGAIN {
                return failed(&mut data, "o decodificador AV1 parou", EAGAIN);
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: closes the context, once; dav1d sets it to None.
        unsafe { dav1d_close(Some(NonNull::from(&mut self.context))) };
    }
}

/// A picture's planes (Y, U, V), and the strides of luma and chroma.
pub type Planes<'a> = ([&'a [u8]; 3], (usize, usize));

/// A decoded picture, kept by the decoder until dropped.
pub struct Picture(Dav1dPicture);

impl Picture {
    pub fn dimensions(&self) -> (usize, usize) {
        (self.0.p.w.max(0) as usize, self.0.p.h.max(0) as usize)
    }

    /// The planes, when the picture is 8-bit 4:2:0 (what the cards'
    /// encoders make).
    pub fn planes(&self) -> Option<Planes<'_>> {
        let p = &self.0;
        if p.p.layout != DAV1D_PIXEL_LAYOUT_I420 || p.p.bpc != 8 {
            return None;
        }
        let (_, h) = self.dimensions();
        let luma = usize::try_from(p.stride[0]).ok()?;
        let chroma = usize::try_from(p.stride[1]).ok()?;
        let (y, u, v) = (p.data[0]?, p.data[1]?, p.data[2]?);
        let half = h.div_ceil(2);
        // SAFETY: while the picture is referenced, dav1d's planes hold `h`
        // rows of `luma` bytes and `h / 2` (rounded up) rows of `chroma`
        // bytes each.
        unsafe {
            Some((
                [
                    std::slice::from_raw_parts(y.as_ptr().cast::<u8>(), luma * h),
                    std::slice::from_raw_parts(u.as_ptr().cast::<u8>(), chroma * half),
                    std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), chroma * half),
                ],
                (luma, chroma),
            ))
        }
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture came from dav1d_get_picture and is let go once.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut self.0))) };
    }
}

/// I420 into RGBA (egui's Color32), in the colours the sending side put
/// in: BT.601, limited range (hw.rs sets the card's encoder up so, as
/// openh264 expects).
pub fn i420_to_rgba(
    planes: [&[u8]; 3],
    (luma, chroma): (usize, usize),
    (w, h): (usize, usize),
    out: &mut [u8],
) {
    for row in 0..h {
        let y = &planes[0][row * luma..][..w];
        let u = &planes[1][row / 2 * chroma..][..w.div_ceil(2)];
        let v = &planes[2][row / 2 * chroma..][..w.div_ceil(2)];
        let out = &mut out[row * w * 4..][..w * 4];
        #[cfg(target_arch = "x86_64")]
        let done = if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 is there (checked above), and the rows have the
            // lengths the function asks for.
            unsafe { row_avx2(y, u, v, out) }
        } else {
            0
        };
        #[cfg(not(target_arch = "x86_64"))]
        let done = 0;
        for x in done..w {
            let px = pixel(y[x], u[x / 2], v[x / 2]);
            out[x * 4..x * 4 + 4].copy_from_slice(&px);
        }
    }
}

/// One pixel, in integers (the usual BT.601 limited-range coefficients,
/// scaled by 256).
fn pixel(y: u8, u: u8, v: u8) -> [u8; 4] {
    let c = (i32::from(y) - 16) * 298;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let clip = |x: i32| ((x + 128) >> 8).clamp(0, 255) as u8;
    [
        clip(c + 409 * e),
        clip(c - 100 * d - 208 * e),
        clip(c + 516 * d),
        255,
    ]
}

/// As `pixel`, eight pixels at a time: each 32-bit lane ends up one RGBA
/// pixel, so there is nothing to interleave. Returns how many pixels of
/// the row it did (a multiple of 8; `pixel` does the rest).
///
/// # Safety
///
/// AVX2 must be there; `y` and `out` hold one row (`out` four bytes a
/// pixel), `u` and `v` its chroma (half as many, rounded up).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn row_avx2(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8]) -> usize {
    use std::arch::x86_64::{
        _mm_cvtsi32_si128, _mm_loadl_epi64, _mm_unpacklo_epi8, _mm256_add_epi32,
        _mm256_cvtepu8_epi32, _mm256_max_epi32, _mm256_min_epi32, _mm256_mullo_epi32,
        _mm256_or_si256, _mm256_set1_epi32, _mm256_setzero_si256, _mm256_slli_epi32,
        _mm256_srai_epi32, _mm256_storeu_si256, _mm256_sub_epi32,
    };
    let w = y.len().min(out.len() / 4);
    let (k16, k128, k298) = (
        _mm256_set1_epi32(16),
        _mm256_set1_epi32(128),
        _mm256_set1_epi32(298),
    );
    let (k409, k100, k208, k516) = (
        _mm256_set1_epi32(409),
        _mm256_set1_epi32(100),
        _mm256_set1_epi32(208),
        _mm256_set1_epi32(516),
    );
    let (zero, max) = (_mm256_setzero_si256(), _mm256_set1_epi32(255));
    let alpha = _mm256_set1_epi32(0xff00_0000_u32 as i32);
    let mut x = 0;
    while x + 8 <= w && x / 2 + 4 <= u.len().min(v.len()) {
        // SAFETY: x + 8 <= w bytes of `y` and pixels of `out`; x / 2 + 4
        // bytes of `u` and `v` (checked by the loop).
        unsafe {
            let yv = _mm256_cvtepu8_epi32(_mm_loadl_epi64(y.as_ptr().add(x).cast()));
            let four = |p: &[u8]| {
                let b = std::ptr::read_unaligned(p.as_ptr().add(x / 2).cast::<[u8; 4]>());
                i32::from_le_bytes(b)
            };
            let (u4, v4) = (_mm_cvtsi32_si128(four(u)), _mm_cvtsi32_si128(four(v)));
            // Each chroma sample twice: two pixels share it.
            let uv = _mm256_cvtepu8_epi32(_mm_unpacklo_epi8(u4, u4));
            let vv = _mm256_cvtepu8_epi32(_mm_unpacklo_epi8(v4, v4));
            let c = _mm256_mullo_epi32(_mm256_sub_epi32(yv, k16), k298);
            let d = _mm256_sub_epi32(uv, k128);
            let e = _mm256_sub_epi32(vv, k128);
            let r = _mm256_add_epi32(c, _mm256_mullo_epi32(e, k409));
            let g = _mm256_sub_epi32(
                _mm256_sub_epi32(c, _mm256_mullo_epi32(d, k100)),
                _mm256_mullo_epi32(e, k208),
            );
            let b = _mm256_add_epi32(c, _mm256_mullo_epi32(d, k516));
            let r = _mm256_min_epi32(
                _mm256_max_epi32(_mm256_srai_epi32::<8>(_mm256_add_epi32(r, k128)), zero),
                max,
            );
            let g = _mm256_min_epi32(
                _mm256_max_epi32(_mm256_srai_epi32::<8>(_mm256_add_epi32(g, k128)), zero),
                max,
            );
            let b = _mm256_min_epi32(
                _mm256_max_epi32(_mm256_srai_epi32::<8>(_mm256_add_epi32(b, k128)), zero),
                max,
            );
            let px = _mm256_or_si256(
                _mm256_or_si256(r, _mm256_slli_epi32::<8>(g)),
                _mm256_or_si256(_mm256_slli_epi32::<16>(b), alpha),
            );
            _mm256_storeu_si256(out.as_mut_ptr().add(x * 4).cast(), px);
        }
        x += 8;
    }
    x
}
